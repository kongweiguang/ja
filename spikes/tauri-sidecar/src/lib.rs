// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! 与生产 Tauri host 隔离的 sidecar 生命周期探针。
//!
//! 这里故意只使用标准库和平台原生 API，避免探针把尚未冻结的 Tauri
//! plugin/异步 runtime 选择带入生产模块。它验证的是进程边界、stdio 和
//! 状态恢复不变量，而不是最终 UI 集成。

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::io::{self, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(unix)]
use std::sync::atomic::AtomicI32;

const DEFAULT_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 256;
const DEFAULT_WRITER_QUEUE_CAPACITY: usize = 64;
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(100);
const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// sidecar 的显式生命周期；将非法组合排除在 bool 标志之外，避免关闭和重启竞态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    Starting,
    Ready,
    Stopping,
    Exited,
    Backoff,
    Incompatible,
}

/// stdout/stderr reader 和进程监视器通过此事件向 host 交付事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarEvent {
    StdoutFrame(String),
    StdoutClosed,
    StderrLine(String),
    ProtocolViolation(ProtocolViolation),
    ProcessExited {
        generation: u64,
        code: Option<i32>,
        success: bool,
    },
    QueueOverflow {
        stream: OutputStream,
    },
    QueueFatalOverflow {
        stream: OutputStream,
    },
}

/// 将污染、半行和帧上限区分开，使上层可以决定是否重启。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolViolation {
    InvalidUtf8,
    InvalidJsonObject,
    UnexpectedEof,
    FrameTooLarge,
    ReaderIo(String),
}

/// 输出来源用于诊断和队列溢出归因，不携带完整环境或 secret。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// 重启策略只允许有限次数与上限退避，版本不兼容永不进入此路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RestartPolicy {
    /// 使用有界退避默认值，避免 crash 后无限重启消耗桌面资源。
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: DEFAULT_BACKOFF_BASE,
            max_delay: DEFAULT_BACKOFF_MAX,
        }
    }
}

/// 子进程启动参数只接收结构化字段；shell 语义和完整继承环境被明确排除。
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    pub initialize_frame: String,
    pub max_frame_bytes: usize,
    pub max_stderr_line_bytes: usize,
    pub event_queue_capacity: usize,
    pub writer_queue_capacity: usize,
    pub ready_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub restart: RestartPolicy,
}

impl SidecarConfig {
    /// 提供协议首发的安全默认值，让调用方只需替换可执行文件和工作目录。
    pub fn new(executable: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            workspace_root: None,
            env: BTreeMap::new(),
            initialize_frame: r#"{"jsonrpc":"2.0","id":"c:initialize","method":"initialize","params":{"protocolMajor":1,"protocolMinor":0,"minimumCompatibleMinor":0,"clientVersion":"sidecar-spike","capabilities":{},"limits":{"maxFrameBytes":4194304}}}"#.to_string(),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_stderr_line_bytes: 64 * 1024,
            event_queue_capacity: DEFAULT_EVENT_QUEUE_CAPACITY,
            writer_queue_capacity: DEFAULT_WRITER_QUEUE_CAPACITY,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            restart: RestartPolicy::default(),
        }
    }

    /// 启动前拒绝 workspace cwd、相对路径和无界队列，避免 sidecar 获得隐式权限。
    pub fn validate(&self) -> Result<(), SidecarError> {
        if !self.executable.is_absolute() {
            return Err(SidecarError::InvalidConfig(
                "executable must be an absolute path".to_string(),
            ));
        }
        if !self.cwd.is_absolute() {
            return Err(SidecarError::InvalidConfig(
                "cwd must be an absolute path".to_string(),
            ));
        }
        let canonical_cwd = std::fs::canonicalize(&self.cwd).map_err(|_| {
            SidecarError::InvalidConfig("cwd must be an existing directory".to_string())
        })?;
        if !canonical_cwd.is_dir() {
            return Err(SidecarError::InvalidConfig(
                "cwd must be an existing directory".to_string(),
            ));
        }
        if let Some(workspace_root) = &self.workspace_root {
            if !workspace_root.is_absolute() {
                return Err(SidecarError::InvalidConfig(
                    "workspace root must be an absolute path".to_string(),
                ));
            }
            let canonical_workspace = std::fs::canonicalize(workspace_root).map_err(|_| {
                SidecarError::InvalidConfig(
                    "workspace root must be an existing directory".to_string(),
                )
            })?;
            if !canonical_workspace.is_dir() {
                return Err(SidecarError::InvalidConfig(
                    "workspace root must be an existing directory".to_string(),
                ));
            }
            if canonical_cwd.starts_with(&canonical_workspace) {
                return Err(SidecarError::InvalidConfig(
                    "sidecar cwd must not be inside the workspace root".to_string(),
                ));
            }
        }
        if self.max_frame_bytes == 0 || self.max_frame_bytes > 16 * 1024 * 1024 {
            return Err(SidecarError::InvalidConfig(
                "max_frame_bytes must be between 1 and 16 MiB".to_string(),
            ));
        }
        if self.max_stderr_line_bytes == 0
            || self.event_queue_capacity == 0
            || self.writer_queue_capacity == 0
        {
            return Err(SidecarError::InvalidConfig(
                "output and queue limits must be non-zero".to_string(),
            ));
        }
        if self.restart.base_delay.is_zero()
            || self.restart.max_delay < self.restart.base_delay
            || self.restart.max_attempts == 0
        {
            return Err(SidecarError::InvalidConfig(
                "restart policy must be bounded and non-zero".to_string(),
            ));
        }
        if self.initialize_frame.as_bytes().contains(&b'\n') {
            return Err(SidecarError::InvalidConfig(
                "initialize frame must be a single JSONL frame".to_string(),
            ));
        }
        Ok(())
    }
}

/// 关闭结果区分 graceful 与强制进程树终止，便于桌面端显示真实恢复状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    Graceful { code: Option<i32> },
    Forced { code: Option<i32> },
}

/// sidecar 管理器只在一个 host 线程上改变 FSM，reader/monitor 只投递事实事件。
pub struct SidecarSupervisor {
    config: SidecarConfig,
    state: LifecycleState,
    events: Arc<BoundedEventQueue>,
    child: Option<Arc<ManagedChild>>,
    next_generation: u64,
    active_generation: Option<u64>,
    restart_attempt: u32,
    next_restart_at: Option<Instant>,
}

impl SidecarSupervisor {
    /// 创建未运行的 supervisor；实际进程只在显式 start 时产生，便于失败前校验配置。
    pub fn new(config: SidecarConfig) -> Result<Self, SidecarError> {
        config.validate()?;
        Ok(Self {
            events: Arc::new(BoundedEventQueue::new(config.event_queue_capacity)),
            config,
            state: LifecycleState::Exited,
            child: None,
            next_generation: 0,
            active_generation: None,
            restart_attempt: 0,
            next_restart_at: None,
        })
    }

    /// 返回权威 FSM 状态；状态只由 supervisor 线程根据事件推进。
    pub fn state(&self) -> LifecycleState {
        self.state
    }

    /// 启动并完成 initialize/ready barrier，未 ready 的 child 会被完整清理。
    pub fn start(&mut self) -> Result<(), SidecarError> {
        if !matches!(self.state, LifecycleState::Exited) {
            return Err(SidecarError::InvalidState(self.state));
        }
        // note_crash_for_restart() 可能在没有先消费 ProcessExited 的情况下被调用；
        // 启动新 generation 前先收口旧 child，避免旧事件/孙进程污染新实例。
        if self.child.is_some() {
            self.fail_and_kill();
        }
        self.next_restart_at = None;
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = self.next_generation;
        self.state = LifecycleState::Starting;
        let child = match ManagedChild::spawn(&self.config, Arc::clone(&self.events), generation) {
            Ok(child) => Arc::new(child),
            Err(error) => {
                self.state = LifecycleState::Exited;
                return Err(error);
            }
        };
        self.child = Some(child);
        self.active_generation = Some(generation);
        if let Err(error) = self.send_frame(&self.config.initialize_frame) {
            self.fail_and_kill();
            return Err(error);
        }
        if let Err(error) = self.wait_for_ready(self.config.ready_timeout) {
            self.fail_and_kill();
            return Err(error);
        }
        self.restart_attempt = 0;
        Ok(())
    }

    /// 发送单帧请求；single-writer 线程负责唯一 stdin 所有权和 flush。
    pub fn send_frame(&self, frame: &str) -> Result<(), SidecarError> {
        validate_frame(frame, self.config.max_frame_bytes)?;
        if matches!(
            self.state,
            LifecycleState::Exited
                | LifecycleState::Stopping
                | LifecycleState::Backoff
                | LifecycleState::Incompatible
        ) {
            return Err(SidecarError::NotRunning);
        }
        let child = self.child.as_ref().ok_or(SidecarError::NotRunning)?;
        child.send_frame(frame.to_string())
    }

    /// 通过 channel/Condvar 获取事件，避免 reader 因 UI 慢消费而阻塞。
    pub fn poll_event(&mut self, timeout: Duration) -> Option<SidecarEvent> {
        let event = self.events.pop_timeout(timeout)?;
        // 先把判定保存为值，再借用 event 做普通 FSM 映射；这样 fatal 收口不会
        // 与返回诊断事件的所有权冲突，也不会让调用方必须记得额外 shutdown。
        let fatal_overflow = matches!(&event, SidecarEvent::QueueFatalOverflow { .. });
        let exited_generation = match &event {
            SidecarEvent::ProcessExited { generation, .. } => Some(*generation),
            _ => None,
        };
        self.apply_event(&event);
        if fatal_overflow {
            self.fail_and_kill();
        }
        if let Some(generation) = exited_generation {
            self.reap_exited_child(generation);
        }
        Some(event)
    }

    /// 请求 graceful shutdown，超过 deadline 后终止 Job/process group 完整树。
    pub fn shutdown(&mut self, deadline: Duration) -> Result<ShutdownOutcome, SidecarError> {
        let Some(child) = self.child.take() else {
            self.active_generation = None;
            self.state = LifecycleState::Exited;
            return Ok(ShutdownOutcome::Graceful { code: None });
        };
        self.active_generation = None;
        self.state = LifecycleState::Stopping;
        let shutdown_frame = r#"{"jsonrpc":"2.0","id":"c:shutdown","method":"shutdown","params":{"reason":"host_shutdown"}}"#;
        let _ = child.send_frame(shutdown_frame.to_string());
        if let Some(status) = child.wait_done(deadline) {
            self.state = LifecycleState::Exited;
            return Ok(ShutdownOutcome::Graceful {
                code: status.code(),
            });
        }
        child.terminate_tree();
        let status = child.wait_done(self.config.shutdown_timeout);
        self.state = LifecycleState::Exited;
        Ok(ShutdownOutcome::Forced {
            code: status.and_then(|value| value.code()),
        })
    }

    /// 对已崩溃的 child 计算有限退避；Incompatible 不允许被误重启成 crash loop。
    pub fn restart(&mut self) -> Result<(), SidecarError> {
        if self.state == LifecycleState::Incompatible {
            return Err(SidecarError::Incompatible);
        }
        if let Some(retry_at) = self.next_restart_at {
            if Instant::now() < retry_at {
                self.state = LifecycleState::Backoff;
                return Err(SidecarError::Backoff { retry_at });
            }
            self.state = LifecycleState::Exited;
        }
        if self.state != LifecycleState::Exited {
            return Err(SidecarError::InvalidState(self.state));
        }
        if self.restart_attempt >= self.config.restart.max_attempts {
            return Err(SidecarError::RestartLimitExceeded);
        }
        self.restart_attempt += 1;
        let attempt = self.restart_attempt;
        let result = self.start();
        if result.is_ok() {
            // start() clears the pending timer; retain the attempt count for the next crash.
            self.restart_attempt = attempt;
        }
        result
    }

    /// 让测试或上层 scheduler 等待退避时点，不使用无界轮询或任意 sleep。
    pub fn wait_until_restartable(&self, timeout: Duration) -> bool {
        let Some(retry_at) = self.next_restart_at else {
            return true;
        };
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= retry_at {
                return true;
            }
            if now >= deadline {
                return false;
            }

            // `park_timeout` may return because of a stale unpark token or a
            // spurious wakeup; recomputing both deadlines prevents that wakeup
            // from being reported as a premature restart opportunity.
            let until_retry = retry_at.saturating_duration_since(now);
            let until_deadline = deadline.saturating_duration_since(now);
            thread::park_timeout(until_retry.min(until_deadline));
        }
    }

    /// 将 child 的 crash 变成 Exited/Backoff 事实，同时保留有限恢复入口。
    pub fn note_crash_for_restart(&mut self) {
        if self.state == LifecycleState::Incompatible {
            return;
        }
        self.state = LifecycleState::Exited;
        let delay = backoff_delay(
            self.config.restart.base_delay,
            self.config.restart.max_delay,
            self.restart_attempt,
        );
        self.next_restart_at = Some(Instant::now() + delay);
        self.state = LifecycleState::Backoff;
    }

    /// 在 drop 或启动失败时清理完整进程树，避免 orphan sidecar 泄漏到桌面会话。
    fn fail_and_kill(&mut self) {
        self.active_generation = None;
        if let Some(child) = self.child.take() {
            child.terminate_tree();
            let _ = child.wait_done(self.config.shutdown_timeout);
        }
        if self.state != LifecycleState::Incompatible {
            self.state = LifecycleState::Exited;
        }
    }

    /// 将事件中的 ready、兼容性和退出事实集中映射为 FSM 转换。
    fn apply_event(&mut self, event: &SidecarEvent) {
        match event {
            SidecarEvent::StdoutFrame(frame) if is_ready_frame(frame) => {
                self.state = LifecycleState::Ready;
            }
            SidecarEvent::StdoutFrame(frame) if is_incompatible_frame(frame) => {
                self.state = LifecycleState::Incompatible;
            }
            SidecarEvent::ProcessExited { generation, .. }
                if !matches!(
                    self.state,
                    LifecycleState::Stopping | LifecycleState::Incompatible
                ) && self.active_generation == Some(*generation) =>
            {
                self.state = LifecycleState::Exited;
            }
            SidecarEvent::QueueFatalOverflow { .. } => {
                // 控制事实无法可靠交付时，继续 Ready 会让 host 误以为 sidecar
                // 仍可收发请求；poll_event 随后同步终止完整进程树。
                self.state = LifecycleState::Exited;
            }
            _ => {}
        }
    }

    /// 消费当前 generation 的退出事实后立即回收 remaining tree；旧 generation
    /// 的迟到事件只保留诊断，不得误杀已重启的新实例。
    fn reap_exited_child(&mut self, generation: u64) {
        if self.active_generation != Some(generation) {
            return;
        }
        self.active_generation = None;
        if let Some(child) = self.child.take() {
            child.terminate_tree();
            let _ = child.wait_done(self.config.shutdown_timeout);
        }
        if self.state != LifecycleState::Incompatible {
            self.state = LifecycleState::Exited;
        }
    }

    /// 等待协议 ready 或不可恢复错误；deadline 只防止错误 fixture 永久挂起。
    fn wait_for_ready(&mut self, timeout: Duration) -> Result<(), SidecarError> {
        let deadline = Instant::now() + timeout;
        let mut process_exit = None;
        let mut pending_protocol = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SidecarError::ReadyTimeout);
            }
            let Some(event) = self.poll_event(remaining) else {
                return Err(SidecarError::ReadyTimeout);
            };
            match event {
                SidecarEvent::StdoutFrame(frame) if is_ready_frame(&frame) => return Ok(()),
                SidecarEvent::StdoutFrame(frame) if is_incompatible_frame(&frame) => {
                    self.state = LifecycleState::Incompatible;
                    return Err(SidecarError::Incompatible);
                }
                SidecarEvent::ProtocolViolation(violation) => {
                    if process_exit.is_some() {
                        // child 已退出时 reader 仍可能尚未交付 ready frame；保留故障，
                        // 等 StdoutClosed 证明 pipe 已排空后再决定最终错误。
                        pending_protocol = Some(violation);
                    } else {
                        return Err(SidecarError::Protocol(violation));
                    }
                }
                SidecarEvent::ProcessExited { code, .. }
                    if self.state == LifecycleState::Exited && self.child.is_none() =>
                {
                    // monitor 与 stdout reader 是两个线程；先记录退出，给 reader 机会
                    // 交付 child 在退出前已经写入 pipe 的 ready frame。
                    process_exit = Some(code);
                }
                SidecarEvent::StdoutClosed => {
                    if let Some(code) = process_exit {
                        if let Some(violation) = pending_protocol {
                            return Err(SidecarError::Protocol(violation));
                        }
                        return Err(SidecarError::ChildExited(code));
                    }
                }
                SidecarEvent::QueueFatalOverflow { .. } => {
                    return Err(SidecarError::QueueFatalOverflow);
                }
                _ => {}
            }
        }
    }
}

impl Drop for SidecarSupervisor {
    /// Drop 仍须回收进程树，因为窗口关闭可能绕过显式 shutdown handler。
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.shutdown(self.config.shutdown_timeout);
        }
    }
}

/// 对外暴露的可诊断错误不包含 argv、完整环境、secret 或绝对用户路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarError {
    InvalidConfig(String),
    InvalidState(LifecycleState),
    NotRunning,
    QueueFull,
    Protocol(ProtocolViolation),
    ReadyTimeout,
    ChildExited(Option<i32>),
    Spawn(String),
    Incompatible,
    Backoff { retry_at: Instant },
    RestartLimitExceeded,
    ShutdownTimeout,
    QueueFatalOverflow,
}

impl Display for SidecarError {
    /// 用稳定分类替代错误中带 secret 的底层命令行和路径。
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfig(reason) => write!(formatter, "invalid sidecar config: {reason}"),
            Self::InvalidState(state) => write!(formatter, "invalid sidecar state: {state:?}"),
            Self::NotRunning => formatter.write_str("sidecar is not running"),
            Self::QueueFull => formatter.write_str("sidecar writer queue is full"),
            Self::Protocol(violation) => {
                write!(formatter, "sidecar protocol violation: {violation:?}")
            }
            Self::ReadyTimeout => formatter.write_str("sidecar ready deadline exceeded"),
            Self::ChildExited(code) => write!(formatter, "sidecar exited before ready: {code:?}"),
            Self::Spawn(reason) => write!(formatter, "sidecar spawn failed: {reason}"),
            Self::Incompatible => formatter.write_str("sidecar protocol is incompatible"),
            Self::Backoff { .. } => formatter.write_str("sidecar restart is in bounded backoff"),
            Self::RestartLimitExceeded => formatter.write_str("sidecar restart limit exceeded"),
            Self::ShutdownTimeout => formatter.write_str("sidecar shutdown deadline exceeded"),
            Self::QueueFatalOverflow => formatter.write_str("sidecar control queue overflowed"),
        }
    }
}

impl std::error::Error for SidecarError {}

struct BoundedEventQueue {
    inner: Mutex<QueueState>,
    wake: Condvar,
    capacity: usize,
    overflow: AtomicBool,
    fatal_overflow: AtomicBool,
    overflow_stream: Mutex<Option<OutputStream>>,
    waiters: AtomicUsize,
}

struct QueueState {
    /// 普通 delta/diagnostic 的 bounded FIFO；不能挤掉控制事实。
    data: VecDeque<SidecarEvent>,
    /// handshake/exit/protocol 事实独立排队，容量不足时转 fatal。
    control: VecDeque<SidecarEvent>,
}

impl BoundedEventQueue {
    /// 以固定容量保存诊断，reader 满载时丢弃而不是反向阻塞 child stdout/stderr。
    fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(QueueState {
                data: VecDeque::with_capacity(capacity),
                control: VecDeque::with_capacity(control_capacity(capacity)),
            }),
            wake: Condvar::new(),
            capacity,
            overflow: AtomicBool::new(false),
            fatal_overflow: AtomicBool::new(false),
            overflow_stream: Mutex::new(None),
            waiters: AtomicUsize::new(0),
        }
    }

    /// 非阻塞入队；队满信号会在后续 poll 中以稳定事件呈现。
    fn push(&self, event: SidecarEvent) {
        let control = is_control_event(&event);
        let stream = match &event {
            SidecarEvent::StdoutFrame(_) => Some(OutputStream::Stdout),
            SidecarEvent::StderrLine(_) => Some(OutputStream::Stderr),
            _ => None,
        };
        let mut queue = if control {
            // 控制事实只在容量耗尽时 fatal；短暂锁竞争必须等待并入队，
            // 否则高频 delta 恰好持锁时会把真实 ProcessExited 误报成故障。
            match self.inner.lock() {
                Ok(queue) => queue,
                Err(_) => {
                    self.mark_fatal_overflow(stream);
                    return;
                }
            }
        } else {
            match self.inner.try_lock() {
                Ok(queue) => queue,
                Err(_) => {
                    self.mark_overflow(stream);
                    return;
                }
            }
        };
        if control {
            if queue.control.len() >= control_capacity(self.capacity) {
                drop(queue);
                self.mark_fatal_overflow(stream);
                return;
            }
            queue.control.push_back(event);
        } else if queue.data.len() >= self.capacity {
            drop(queue);
            self.mark_overflow(stream);
            return;
        } else {
            queue.data.push_back(event);
        }
        self.wake.notify_one();
    }

    /// 循环检查事实与 overflow，避免 wait 唤醒后把溢出误报成 None。
    fn pop_timeout(&self, timeout: Duration) -> Option<SidecarEvent> {
        let deadline = Instant::now() + timeout;
        let mut queue = self.inner.lock().ok()?;
        loop {
            if self.fatal_overflow.swap(false, Ordering::AcqRel) {
                return Some(SidecarEvent::QueueFatalOverflow {
                    stream: self.take_overflow_stream(),
                });
            }
            if self.overflow.swap(false, Ordering::AcqRel) {
                return Some(SidecarEvent::QueueOverflow {
                    stream: self.take_overflow_stream(),
                });
            }
            if let Some(event) = queue.control.pop_front() {
                return Some(event);
            }
            if let Some(event) = queue.data.pop_front() {
                return Some(event);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            // 记录等待者数量既便于诊断，也让测试可以用同步屏障确认已经进入
            // Condvar，而不是依赖任意 sleep 猜测通知时序。
            self.waiters.fetch_add(1, Ordering::AcqRel);
            let waited = self.wake.wait_timeout(queue, remaining);
            self.waiters.fetch_sub(1, Ordering::AcqRel);
            let Ok((guard, result)) = waited else {
                return None;
            };
            queue = guard;
            if result.timed_out() {
                // overflow/控制事实可能与 timeout 同时到达；回到循环先重新
                // 检查原子标志，避免把可观察故障错误地降级为 None。
                continue;
            }
        }
    }

    /// 记录溢出流并只保留一个标志，防止诊断本身造成无限队列。
    fn mark_overflow(&self, stream: Option<OutputStream>) {
        self.overflow.store(true, Ordering::Release);
        self.set_overflow_stream(stream);
        self.wake.notify_one();
    }

    /// 控制事实不能静默丢失；控制队列满时进入可观察 fatal 状态并由 host fail-closed。
    fn mark_fatal_overflow(&self, stream: Option<OutputStream>) {
        self.fatal_overflow.store(true, Ordering::Release);
        // fatal 事件可能来自无 stream 的 ProcessExited/ready；覆盖旧诊断来源，
        // 避免把上一轮 stderr 普通 overflow 错报成当前控制队列故障。
        if let Ok(mut value) = self.overflow_stream.lock() {
            *value = Some(stream.unwrap_or(OutputStream::Stdout));
        }
        self.wake.notify_one();
    }

    /// 只记录有限诊断来源，避免 overflow 自己扩展成无界队列。
    fn set_overflow_stream(&self, stream: Option<OutputStream>) {
        if let Some(stream) = stream {
            if let Ok(mut value) = self.overflow_stream.lock() {
                *value = Some(stream);
            }
        }
    }

    /// 读取并清空最近一次 overflow 来源，缺省按 stdout 处理以 fail-closed。
    fn take_overflow_stream(&self) -> OutputStream {
        self.overflow_stream
            .lock()
            .ok()
            .and_then(|mut value| value.take())
            .unwrap_or(OutputStream::Stdout)
    }
}

/// 为控制事实预留独立有限槽位，普通输出再多也不会挤掉握手/退出事件。
fn control_capacity(data_capacity: usize) -> usize {
    data_capacity.clamp(4, 32)
}

struct ManagedChild {
    writer: mpsc::SyncSender<String>,
    tree: ProcessTreeGuard,
    done: Arc<(Mutex<Option<ExitStatus>>, Condvar)>,
}

impl ManagedChild {
    /// 创建 pipe、进程组/Job Object 和 reader threads，保证所有输出都有 drain owner。
    fn spawn(
        config: &SidecarConfig,
        events: Arc<BoundedEventQueue>,
        generation: u64,
    ) -> Result<Self, SidecarError> {
        let mut command = Command::new(&config.executable);
        command
            .args(&config.args)
            .current_dir(&config.cwd)
            .env_clear()
            .envs(config.env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let tree = ProcessTreeGuard::prepare(&mut command)
            .map_err(|error| SidecarError::Spawn(error.to_string()))?;
        let mut child = command
            .spawn()
            .map_err(|error| SidecarError::Spawn(error.to_string()))?;
        if let Err(error) = tree.assign(&child).and_then(|_| tree.resume(&child)) {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::Spawn(error.to_string()));
        }
        let Some(stdin) = child.stdin.take() else {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::Spawn("stdin pipe unavailable".to_string()));
        };
        let Some(stdout) = child.stdout.take() else {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::Spawn("stdout pipe unavailable".to_string()));
        };
        let Some(stderr) = child.stderr.take() else {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::Spawn("stderr pipe unavailable".to_string()));
        };

        let (writer, writer_rx) = mpsc::sync_channel(config.writer_queue_capacity);
        if let Err(error) = spawn_writer_thread(stdin, writer_rx) {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::Spawn(error.to_string()));
        }
        if let Err(error) = spawn_stdout_reader(stdout, Arc::clone(&events), config.max_frame_bytes)
        {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::Spawn(error.to_string()));
        }
        if let Err(error) =
            spawn_stderr_reader(stderr, Arc::clone(&events), config.max_stderr_line_bytes)
        {
            tree.terminate();
            let _ = child.kill();
            let _ = child.wait();
            return Err(SidecarError::Spawn(error.to_string()));
        }

        let done = Arc::new((Mutex::new(None), Condvar::new()));
        let done_for_monitor = Arc::clone(&done);
        let events_for_monitor = Arc::clone(&events);
        let tree_for_monitor = tree.clone_handle();
        let child_slot = Arc::new(Mutex::new(Some(child)));
        let child_slot_for_monitor = Arc::clone(&child_slot);
        let monitor = thread::Builder::new()
            .name("ja-sidecar-wait".to_string())
            .spawn(move || {
                let Some(mut child) = child_slot_for_monitor
                    .lock()
                    .ok()
                    .and_then(|mut value| value.take())
                else {
                    return;
                };
                let status = child.wait().ok();
                tree_for_monitor.mark_exited();
                let (lock, wake) = &*done_for_monitor;
                if let Ok(mut value) = lock.lock() {
                    *value = status;
                    wake.notify_all();
                }
                // wait error 也必须变成退出事实；否则 supervisor 会永远保留 child
                // writer，crash 后 send_frame 可能继续入队而无法触发 tree cleanup。
                events_for_monitor.push(SidecarEvent::ProcessExited {
                    generation,
                    code: status.and_then(|value| value.code()),
                    success: status.is_some_and(|value| value.success()),
                });
            });
        if let Err(error) = monitor {
            tree.terminate();
            if let Ok(mut value) = child_slot.lock() {
                if let Some(mut child) = value.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
            return Err(SidecarError::Spawn(error.to_string()));
        }

        Ok(Self { writer, tree, done })
    }

    /// 将一帧交给唯一 writer owner，禁止业务线程直接竞争 stdin。
    fn send_frame(&self, frame: String) -> Result<(), SidecarError> {
        self.writer.try_send(frame).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => SidecarError::QueueFull,
            mpsc::TrySendError::Disconnected(_) => SidecarError::NotRunning,
        })
    }

    /// 在 deadline 内等待 monitor 记录退出事实，不轮询进程句柄。
    fn wait_done(&self, timeout: Duration) -> Option<ExitStatus> {
        let (lock, wake) = &*self.done;
        let value = lock.lock().ok()?;
        if let Some(status) = *value {
            return Some(status);
        }
        let (value, _) = wake.wait_timeout(value, timeout).ok()?;
        *value
    }

    /// 终止 Job Object/process group，保证孙进程随 sidecar 一起退出。
    fn terminate_tree(&self) {
        self.tree.terminate();
    }
}

/// 启动唯一 stdin writer，令协议帧不会被并发业务线程交错写入。
fn spawn_writer_thread(mut stdin: ChildStdin, receiver: mpsc::Receiver<String>) -> io::Result<()> {
    // writer thread 独占 stdin，避免半帧交错；关闭 channel 即关闭 pipe。
    thread::Builder::new()
        .name("ja-sidecar-stdin".to_string())
        .spawn(move || {
            while let Ok(frame) = receiver.recv() {
                if stdin.write_all(frame.as_bytes()).is_err() {
                    break;
                }
                if stdin.write_all(b"\n").is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        })
        .map(|_| ())
}

/// 启动 stdout reader，把 framing 校验与业务 dispatcher 解耦以避免阻塞 child。
fn spawn_stdout_reader(
    stdout: impl Read + Send + 'static,
    events: Arc<BoundedEventQueue>,
    max: usize,
) -> io::Result<()> {
    // stdout 只接受一帧一行；读线程不等待消费方，避免协议 deadlock。
    thread::Builder::new()
        .name("ja-sidecar-stdout".to_string())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_limited_line(&mut reader, max) {
                    Ok(Some(bytes)) => match String::from_utf8(bytes) {
                        Ok(frame) => match validate_frame(&frame, max) {
                            Ok(()) => events.push(SidecarEvent::StdoutFrame(frame)),
                            Err(SidecarError::Protocol(violation)) => {
                                events.push(SidecarEvent::ProtocolViolation(violation))
                            }
                            Err(_) => events.push(SidecarEvent::ProtocolViolation(
                                ProtocolViolation::InvalidJsonObject,
                            )),
                        },
                        Err(_) => events.push(SidecarEvent::ProtocolViolation(
                            ProtocolViolation::InvalidUtf8,
                        )),
                    },
                    Ok(None) => break,
                    Err(ProtocolViolation::UnexpectedEof) => {
                        events.push(SidecarEvent::ProtocolViolation(
                            ProtocolViolation::UnexpectedEof,
                        ));
                        break;
                    }
                    Err(violation) => {
                        events.push(SidecarEvent::ProtocolViolation(violation));
                        break;
                    }
                }
            }
            events.push(SidecarEvent::StdoutClosed);
        })
        .map(|_| ())
}

/// 启动 stderr drain reader，诊断输出满载时丢弃而不是堵住 Java/native child。
fn spawn_stderr_reader(
    stderr: impl Read + Send + 'static,
    events: Arc<BoundedEventQueue>,
    max: usize,
) -> io::Result<()> {
    // stderr 必须持续 drain，但其内容只进有界诊断队列，避免阻塞 JVM。
    thread::Builder::new()
        .name("ja-sidecar-stderr".to_string())
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            loop {
                match read_limited_line(&mut reader, max) {
                    Ok(Some(bytes)) => {
                        let line = String::from_utf8_lossy(&bytes).into_owned();
                        events.push(SidecarEvent::StderrLine(line));
                    }
                    Ok(None) => break,
                    Err(ProtocolViolation::FrameTooLarge) => {
                        events.push(SidecarEvent::QueueOverflow {
                            stream: OutputStream::Stderr,
                        });
                        break;
                    }
                    Err(_) => break,
                }
            }
        })
        .map(|_| ())
}

/// 读取有限长度的 LF 行，避免恶意输出在 UTF-8 校验前耗尽内存。
fn read_limited_line(
    reader: &mut impl Read,
    max: usize,
) -> Result<Option<Vec<u8>>, ProtocolViolation> {
    let mut line = Vec::with_capacity(max.min(4096));
    let mut byte = [0_u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => return Err(ProtocolViolation::UnexpectedEof),
            Ok(_) => {
                line.push(byte[0]);
                if line.len() > max + 1 {
                    return Err(ProtocolViolation::FrameTooLarge);
                }
                if byte[0] == b'\n' {
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    return Ok(Some(line));
                }
            }
            Err(error) => return Err(ProtocolViolation::ReaderIo(error.to_string())),
        }
    }
}

/// 使用有限结构检查保证 stdout 污染和半行不会进入协议 dispatcher。
fn validate_frame(frame: &str, max: usize) -> Result<(), SidecarError> {
    if frame.len() > max {
        return Err(SidecarError::Protocol(ProtocolViolation::FrameTooLarge));
    }
    if !looks_like_json_object(frame) {
        return Err(SidecarError::Protocol(ProtocolViolation::InvalidJsonObject));
    }
    Ok(())
}

/// 用成熟 parser 确认 frame 是完整 JSON object，避免 `{garbage}` 穿过 framing 层。
fn looks_like_json_object(value: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(value)
        .map(|parsed| parsed.is_object())
        .unwrap_or(false)
}

/// 识别 runtime ready barrier，只有 server 明确 ready 才允许业务请求。
fn is_ready_frame(frame: &str) -> bool {
    let Ok(parsed) = parse_object(frame) else {
        return false;
    };
    parsed.get("jsonrpc").and_then(serde_json::Value::as_str) == Some("2.0")
        && parsed.get("id").is_none()
        && parsed.get("method").and_then(serde_json::Value::as_str) == Some("runtime/statusChanged")
        && parsed
            .pointer("/params/status")
            .and_then(serde_json::Value::as_str)
            == Some("ready")
}

/// 退出、ready 和 framing 故障必须优先于普通 delta，才能在有界队列下完成握手。
/// 识别必须在有界队列中保留的控制事件，防止 delta 挤掉退出/握手事实。
fn is_control_event(event: &SidecarEvent) -> bool {
    match event {
        SidecarEvent::StdoutClosed
        | SidecarEvent::ProtocolViolation(_)
        | SidecarEvent::ProcessExited { .. } => true,
        SidecarEvent::StdoutFrame(frame) => is_ready_frame(frame) || is_incompatible_frame(frame),
        SidecarEvent::StderrLine(_)
        | SidecarEvent::QueueOverflow { .. }
        | SidecarEvent::QueueFatalOverflow { .. } => false,
    }
}

/// 识别不可恢复的版本拒绝，阻止 supervisor 将它当作普通 crash 重启。
fn is_incompatible_frame(frame: &str) -> bool {
    let Ok(parsed) = parse_object(frame) else {
        return false;
    };
    parsed.get("jsonrpc").and_then(serde_json::Value::as_str) == Some("2.0")
        && parsed.get("id").and_then(serde_json::Value::as_str) == Some("c:initialize")
        && parsed
            .get("error")
            .and_then(serde_json::Value::as_object)
            .is_some()
        && parsed
            .pointer("/error/code")
            .and_then(serde_json::Value::as_i64)
            == Some(-32003)
        && parsed
            .pointer("/error/data/jaCode")
            .and_then(serde_json::Value::as_str)
            == Some("PROTOCOL_VERSION_UNSUPPORTED")
}

/// 将 frame 解析为 object，握手判定复用同一严格 parser 而不是 substring 搜索。
fn parse_object(frame: &str) -> Result<serde_json::Value, ()> {
    serde_json::from_str::<serde_json::Value>(frame)
        .ok()
        .filter(serde_json::Value::is_object)
        .ok_or(())
}

/// 计算不溢出且不超过上限的指数退避时长。
fn backoff_delay(base: Duration, maximum: Duration, attempt: u32) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
    base.saturating_mul(multiplier).min(maximum)
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;

#[cfg(windows)]
#[derive(Clone)]
struct ProcessTreeGuard {
    handle: Arc<WinJobHandle>,
    alive: Arc<AtomicBool>,
}

#[cfg(windows)]
struct WinJobHandle(*mut std::ffi::c_void);

// SAFETY: a Job Object handle is an OS-owned synchronization/resource handle;
// all operations are thread-safe kernel calls and ownership is retained by Arc.
#[cfg(windows)]
unsafe impl Send for WinJobHandle {}
// SAFETY: see the Send rationale; the handle itself has no Rust aliasing state.
#[cfg(windows)]
unsafe impl Sync for WinJobHandle {}

#[cfg(windows)]
impl Drop for WinJobHandle {
    /// 关闭带 KILL_ON_JOB_CLOSE 的 Job Object，覆盖未被 wait 到的孙进程。
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: handle is created by CreateJobObjectW and owned by this Arc.
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(windows)]
impl ProcessTreeGuard {
    /// 先创建 Job 并以 suspended-start 配置 child，确保 resume 前没有用户代码执行。
    fn prepare(command: &mut Command) -> io::Result<Self> {
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        // SAFETY: null security/name requests an unnamed kernel Job Object.
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits = ExtendedLimitInformation::default();
        limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits is the documented fixed-size structure for info class 9.
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut limits as *mut ExtendedLimitInformation).cast(),
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if ok == 0 {
            // SAFETY: handle was successfully created and is not shared yet.
            unsafe { CloseHandle(handle) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: Arc::new(WinJobHandle(handle)),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }

    /// 将刚创建的 process 加入 Job，Job 会自动覆盖其后代。
    fn assign(&self, child: &Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: both handles are valid for the duration of this call.
        let ok = unsafe { AssignProcessToJobObject(self.handle.0, child.as_raw_handle()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// 只有 Job 关联成功后才恢复 primary thread，消除 spawn→assign 的孙进程竞态。
    fn resume(&self, child: &Child) -> io::Result<()> {
        resume_suspended_process(child.id())
    }

    /// 强制终止整个 Job，而非只杀 Java 主进程。
    fn terminate(&self) {
        if self.alive.load(Ordering::Acquire) {
            // SAFETY: handle remains owned by Arc until ManagedChild drops.
            unsafe { TerminateJobObject(self.handle.0, 1) };
        }
    }

    /// 保留 Job ownership，因父进程退出后仍可能有孙进程需要由 supervisor 终止。
    fn mark_exited(&self) {
        // parent exit does not imply an empty Job; keep the handle active so the
        // supervisor can terminate surviving descendants when it consumes the event.
    }

    /// 将 Job ownership 共享给 wait monitor，同时保持最后一个 Arc 关闭句柄。
    fn clone_handle(&self) -> Self {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    /// 锁竞争只应短暂阻塞控制事件，不能把真实退出事实降级为普通 overflow。
    fn control_event_lock_contention_preserves_fact() {
        let queue = Arc::new(BoundedEventQueue::new(1));
        let lock = queue.inner.lock().expect("queue lock");
        let producer_queue = Arc::clone(&queue);
        let producer = thread::spawn(move || {
            producer_queue.push(SidecarEvent::ProcessExited {
                generation: 0,
                code: Some(17),
                success: false,
            });
        });
        drop(lock);
        producer.join().expect("producer");

        assert_eq!(
            queue.pop_timeout(Duration::from_millis(100)),
            Some(SidecarEvent::ProcessExited {
                generation: 0,
                code: Some(17),
                success: false,
            })
        );
    }

    #[test]
    /// overflow 唤醒等待者后必须返回诊断事件，而不是把通知误报为空队列。
    fn overflow_wakes_waiter_without_returning_none() {
        let queue = Arc::new(BoundedEventQueue::new(1));
        let (ready_sender, ready_receiver) = sync_channel(1);
        let waiter_queue = Arc::clone(&queue);
        let waiter = thread::spawn(move || {
            ready_sender.send(()).expect("waiter started");
            waiter_queue.pop_timeout(Duration::from_secs(2))
        });
        ready_receiver.recv().expect("waiter barrier");

        let deadline = Instant::now() + Duration::from_secs(1);
        while queue.waiters.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "waiter did not enter condvar");
            thread::yield_now();
        }
        queue.mark_overflow(Some(OutputStream::Stdout));

        assert_eq!(
            waiter.join().expect("waiter").expect("overflow event"),
            SidecarEvent::QueueOverflow {
                stream: OutputStream::Stdout,
            }
        );
    }

    #[test]
    /// 旧 generation 的退出事实只能诊断，不能把当前重启实例切回 Exited。
    fn stale_process_exit_does_not_change_active_generation() {
        let config = SidecarConfig::new(
            if cfg!(windows) {
                PathBuf::from(r"C:\ja-test-sidecar.exe")
            } else {
                PathBuf::from("/ja-test-sidecar")
            },
            std::env::temp_dir(),
        );
        let mut supervisor = SidecarSupervisor::new(config).expect("test config");
        supervisor.state = LifecycleState::Ready;
        supervisor.active_generation = Some(2);
        supervisor.events.push(SidecarEvent::ProcessExited {
            generation: 1,
            code: Some(17),
            success: false,
        });

        let event = supervisor
            .poll_event(Duration::from_millis(100))
            .expect("stale exit event");
        assert!(matches!(
            event,
            SidecarEvent::ProcessExited { generation: 1, .. }
        ));
        assert_eq!(supervisor.state, LifecycleState::Ready);
        assert_eq!(supervisor.active_generation, Some(2));
    }

    #[test]
    /// 预置 unpark token 后仍必须按绝对退避时点重新检查，避免伪唤醒造成假阴性。
    fn wait_until_restartable_rechecks_after_early_unpark() {
        let config = SidecarConfig::new(
            if cfg!(windows) {
                PathBuf::from(r"C:\ja-test-sidecar.exe")
            } else {
                PathBuf::from("/ja-test-sidecar")
            },
            std::env::temp_dir(),
        );
        let mut supervisor = SidecarSupervisor::new(config).expect("test config");
        supervisor.next_restart_at = Some(Instant::now() + Duration::from_millis(50));

        // A token left by an unrelated notifier is the deterministic analogue
        // of an early wakeup; the deadline loop must not trust that wakeup.
        thread::current().unpark();

        assert!(supervisor.wait_until_restartable(Duration::from_secs(1)));
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
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut std::ffi::c_void;
    fn Thread32First(snapshot: *mut std::ffi::c_void, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: *mut std::ffi::c_void, entry: *mut ThreadEntry32) -> i32;
    fn OpenThread(access: u32, inherit_handle: i32, thread_id: u32) -> *mut std::ffi::c_void;
    fn ResumeThread(thread: *mut std::ffi::c_void) -> u32;
}

#[cfg(windows)]
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
#[cfg(windows)]
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
#[cfg(windows)]
const INVALID_HANDLE_VALUE: *mut std::ffi::c_void = (-1_isize) as *mut std::ffi::c_void;

#[cfg(windows)]
#[repr(C)]
struct ThreadEntry32 {
    size: u32,
    usage: u32,
    thread_id: u32,
    owner_process_id: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}

#[cfg(windows)]
impl Default for ThreadEntry32 {
    /// 将 dwSize 设为结构大小，满足 Toolhelp32First 的版本化 ABI 要求。
    fn default() -> Self {
        Self {
            size: std::mem::size_of::<Self>() as u32,
            usage: 0,
            thread_id: 0,
            owner_process_id: 0,
            base_priority: 0,
            delta_priority: 0,
            flags: 0,
        }
    }
}

#[cfg(windows)]
/// 恢复 suspended child 的唯一 primary thread；失败由 caller kill/wait 收口。
fn resume_suspended_process(process_id: u32) -> io::Result<()> {
    // SAFETY: snapshot handle and entry point follow Toolhelp32 documented ABI.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry = ThreadEntry32::default();
    // SAFETY: entry is writable and its size field is initialized.
    let mut found = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    let mut result = Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended sidecar thread not found",
    ));
    while found {
        if entry.owner_process_id == process_id {
            // SAFETY: thread id came from the current Toolhelp snapshot.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id) };
            if thread.is_null() {
                result = Err(io::Error::last_os_error());
            } else {
                // SAFETY: the handle has THREAD_SUSPEND_RESUME access and is closed below.
                let previous = unsafe { ResumeThread(thread) };
                // SAFETY: thread is a valid kernel handle returned by OpenThread.
                unsafe { CloseHandle(thread) };
                if previous == u32::MAX {
                    result = Err(io::Error::last_os_error());
                } else {
                    result = Ok(());
                }
            }
            break;
        }
        // SAFETY: snapshot remains valid until the final CloseHandle.
        found = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: snapshot is a valid handle from CreateToolhelp32Snapshot.
    unsafe { CloseHandle(snapshot) };
    result
}

#[cfg(unix)]
#[derive(Clone)]
struct ProcessTreeGuard {
    pid: Arc<AtomicI32>,
    alive: Arc<AtomicBool>,
}

#[cfg(unix)]
impl ProcessTreeGuard {
    /// Unix/macOS 在 exec 前建立独立 process group，后续 killpg 覆盖整个 sidecar 树。
    fn prepare(command: &mut Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        // SAFETY: pre_exec runs in the child before exec and only calls async-signal-safe setpgid.
        unsafe {
            command.pre_exec(|| {
                if setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
        }
        Ok(Self {
            pid: Arc::new(AtomicI32::new(0)),
            alive: Arc::new(AtomicBool::new(true)),
        })
    }

    /// 记录实际 pid；spawn 后调用，避免把 pid 拼入任何日志或错误文本。
    fn assign(&self, child: &Child) -> io::Result<()> {
        self.pid.store(child.id() as i32, Ordering::Release);
        Ok(())
    }

    /// Unix process-group ownership is established in pre_exec, so no suspended resume is needed.
    fn resume(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    /// 通过进程组信号终止 Java/native child 及其孙进程。
    fn terminate(&self) {
        let pid = self.pid.load(Ordering::Acquire);
        if self.alive.load(Ordering::Acquire) && pid > 0 {
            // SAFETY: negative pid targets the process group created by pre_exec.
            unsafe {
                let _ = kill(-pid, SIGTERM);
                let _ = kill(-pid, SIGKILL);
            }
        }
    }

    fn mark_exited(&self) {
        // Keep the group eligible for drop cleanup: a parent can exit while a grandchild remains.
    }

    /// 复制 process-group guard，让 monitor 与 supervisor 共享清理状态。
    fn clone_handle(&self) -> Self {
        self.clone()
    }
}

#[cfg(unix)]
impl Drop for ProcessTreeGuard {
    /// parent 已退出时仍清理同组孙进程，避免 orphan；不存在的 group 会安全返回错误。
    fn drop(&mut self) {
        if self.alive.load(Ordering::Acquire) {
            self.terminate();
            self.alive.store(false, Ordering::Release);
        }
    }
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn setpgid(pid: i32, pgid: i32) -> i32;
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone)]
struct ProcessTreeGuard;

#[cfg(not(any(unix, windows)))]
impl ProcessTreeGuard {
    fn prepare(_command: &mut Command) -> io::Result<Self> {
        Ok(Self)
    }

    fn assign(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    /// 未支持平台保持统一 spawn API；真实树清理仍由目标平台实现提供。
    fn resume(&self, _child: &Child) -> io::Result<()> {
        Ok(())
    }

    fn terminate(&self) {}

    fn mark_exited(&self) {}

    /// 保持未支持平台的编译路径可用，生产目标仍由 Windows/Unix 实现提供清理。
    fn clone_handle(&self) -> Self {
        self.clone()
    }
}
