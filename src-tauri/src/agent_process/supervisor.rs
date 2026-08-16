// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Sidecar lifecycle orchestration.
//!
//! Configuration, client handles, and child ownership live in dedicated sibling
//! modules. This module is the sole lifecycle transition owner, which keeps
//! stale terminal callbacks from reviving a newer process generation.

use crate::agent_process::codec::{self, RpcFrame};
use crate::agent_process::error::AgentProcessError;
use crate::agent_process::handshake::{
    MAX_READY_TIMEOUT, MAX_SHUTDOWN_TIMEOUT, bounded_string, checked_deadline,
    error_is_incompatible, generate_ready_token, is_ready_notification,
    is_runtime_ready_notification, valid_schema_id, valid_version, validate_capabilities,
    validate_remote_limits, validate_workspace_policy,
};
use crate::agent_process::lifecycle::{LifecycleMachine, LifecycleState};
use crate::agent_process::session::{EventPump, Session, SessionEvent, TerminalReason};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[path = "client.rs"]
mod client;
#[path = "config.rs"]
mod config;
#[path = "process.rs"]
mod process;

pub use client::AgentClient;
pub use config::SidecarConfig;
use process::{RunningProcess, TerminalSignal, spawn_process};

/// 唯一控制真实 sidecar 的宿主状态；所有状态变更回到 lifecycle 单线程对象。
pub struct SidecarSupervisor {
    config: SidecarConfig,
    lifecycle: LifecycleMachine,
    process: Option<Arc<RunningProcess>>,
    session: Option<Session>,
    event_pump: Option<EventPump>,
    expected_server_instance: Option<String>,
    terminal_signals: Arc<Mutex<VecDeque<TerminalSignal>>>,
    stopping: Arc<Mutex<bool>>,
}

impl SidecarSupervisor {
    /// 校验边界后才创建生命周期 owner，避免无效配置进入 crash-loop。
    pub fn new(config: SidecarConfig) -> Result<Self, AgentProcessError> {
        config.validate()?;
        let lifecycle = LifecycleMachine::new(config.restart)?;
        Ok(Self {
            config,
            lifecycle,
            process: None,
            session: None,
            event_pump: None,
            expected_server_instance: None,
            terminal_signals: Arc::new(Mutex::new(VecDeque::new())),
            stopping: Arc::new(Mutex::new(false)),
        })
    }

    /// 暴露生命周期快照并先归属 terminal signal，避免 UI 不消费 event 时状态仍停在 Ready。
    pub fn state(&mut self) -> LifecycleState {
        self.sync_terminal_signals();
        self.lifecycle.state()
    }

    /// 返回当前 generation，供外部把 response/event 绑定到同一 sidecar 实例。
    pub fn generation(&self) -> u64 {
        self.lifecycle.generation()
    }

    /// 返回不持有 supervisor 锁的 cloneable client，允许多个 thread 并行 request。
    pub fn client(&mut self) -> Result<AgentClient, AgentProcessError> {
        self.sync_terminal_signals();
        if *self
            .stopping
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(AgentProcessError::ShuttingDown);
        }
        if !matches!(
            self.lifecycle.state(),
            LifecycleState::Ready | LifecycleState::Busy
        ) {
            return Err(match self.lifecycle.state() {
                LifecycleState::Stopping => AgentProcessError::ShuttingDown,
                _ => AgentProcessError::NotReady,
            });
        }
        let session = self.session.clone().ok_or(AgentProcessError::NotReady)?;
        Ok(AgentClient {
            session,
            generation: self.lifecycle.generation(),
            stopping: Arc::clone(&self.stopping),
        })
    }

    /// 为当前 generation 生成不可预测 challenge；ready 只接受本 session 的精确值。
    fn next_ready_token(&self) -> Result<String, AgentProcessError> {
        generate_ready_token()
    }

    /// 启动一个新 generation，并在返回前完成严格 initialize/ready 握手。
    pub fn start(&mut self) -> Result<(), AgentProcessError> {
        self.sync_terminal_signals();
        *self
            .stopping
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        if matches!(
            self.lifecycle.state(),
            LifecycleState::Exited | LifecycleState::Backoff
        ) && (self.process.is_some() || self.session.is_some())
        {
            self.fail_process_only();
        }
        let generation = self.lifecycle.begin_start()?;
        self.expected_server_instance = None;
        let (process, session) =
            match spawn_process(&self.config, generation, Arc::clone(&self.terminal_signals)) {
                Ok(value) => value,
                Err(error) => {
                    let _ = self.lifecycle.mark_faulted(generation);
                    return Err(error);
                }
            };
        self.process = Some(process);
        self.session = Some(session.clone());
        self.event_pump = match session.take_event_pump() {
            Ok(pump) => Some(pump),
            Err(error) => {
                self.fail_generation(generation);
                return Err(error);
            }
        };

        let ready_token = match self.next_ready_token() {
            Ok(token) => token,
            Err(error) => {
                self.fail_generation(generation);
                return Err(error);
            }
        };
        if let Err(error) = session.install_ready_token_challenge(ready_token) {
            self.fail_generation(generation);
            return Err(error);
        }

        let initialize = match session.request(
            "initialize",
            self.config.initialize_params.clone(),
            self.config.ready_timeout,
        ) {
            Ok(response) => response,
            Err(error) => {
                self.fail_generation(generation);
                return Err(error);
            }
        };
        if let Some(error) = initialize.error() {
            let incompatible = error_is_incompatible(error.code(), error.data());
            self.fail_process_only();
            if incompatible {
                let _ = self.lifecycle.mark_incompatible(generation);
                return Err(AgentProcessError::Incompatible);
            }
            let _ = self.lifecycle.mark_faulted(generation);
            return Err(AgentProcessError::ProtocolFault);
        }
        let result = initialize
            .result()
            .value()
            .ok_or(AgentProcessError::ProtocolFault)
            .and_then(|result| self.check_initialize_result(result));
        if let Err(error) = result {
            let incompatible = matches!(error, AgentProcessError::Incompatible);
            self.fail_process_only();
            if incompatible {
                let _ = self.lifecycle.mark_incompatible(generation);
            } else {
                let _ = self.lifecycle.mark_faulted(generation);
            }
            return Err(error);
        }

        let initialized_params = match session.initialized_params() {
            Ok(params) => params,
            Err(error) => {
                self.fail_generation(generation);
                return Err(error);
            }
        };
        if let Err(error) = session.notify("initialized", initialized_params) {
            self.fail_generation(generation);
            return Err(error);
        }
        let deadline = match checked_deadline(self.config.ready_timeout, MAX_READY_TIMEOUT) {
            Ok(deadline) => deadline,
            Err(error) => {
                self.fail_generation(generation);
                return Err(error);
            }
        };
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.fail_generation(generation);
                return Err(AgentProcessError::DeadlineExceeded);
            }
            match self
                .event_pump
                .as_mut()
                .and_then(|pump| pump.next_event(remaining))
            {
                Some(SessionEvent::Notification(frame))
                    if is_runtime_ready_notification(&frame) =>
                {
                    if !is_ready_notification(&frame, self.expected_server_instance.as_deref()) {
                        self.fail_generation(generation);
                        return Err(AgentProcessError::HandshakeFailed);
                    }
                    let promotion = session
                        .with_ready_promotion(&frame, || self.lifecycle.mark_ready(generation));
                    if let Err(error) = promotion {
                        self.fail_generation(generation);
                        return Err(error);
                    }
                    return Ok(());
                }
                Some(SessionEvent::ServerRequest(frame)) => {
                    let _ = session.respond_error(
                        frame.id(),
                        -32_004,
                        "not initialized",
                        "NOT_INITIALIZED",
                        false,
                    );
                }
                Some(SessionEvent::ProcessExited { .. } | SessionEvent::Eof) => {
                    self.fail_generation(generation);
                    return Err(AgentProcessError::ProcessExited);
                }
                Some(SessionEvent::HandshakeFailed) => {
                    self.fail_generation(generation);
                    return Err(AgentProcessError::HandshakeFailed);
                }
                Some(
                    SessionEvent::ProtocolFault(_)
                    | SessionEvent::QueueFatalOverflow(_)
                    | SessionEvent::ResponseRejected(_),
                ) => {
                    self.fail_generation(generation);
                    return Err(AgentProcessError::ProtocolFault);
                }
                Some(_) => {}
                None => {
                    self.fail_generation(generation);
                    return Err(AgentProcessError::DeadlineExceeded);
                }
            }
        }
    }

    /// 用 client-compatible path 提供一个短生命周期的 supervisor request。
    pub fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<RpcFrame, AgentProcessError> {
        self.sync_terminal_signals();
        if *self
            .stopping
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(AgentProcessError::ShuttingDown);
        }
        if self.lifecycle.state() != LifecycleState::Ready {
            return Err(match self.lifecycle.state() {
                LifecycleState::Stopping => AgentProcessError::ShuttingDown,
                _ => AgentProcessError::NotReady,
            });
        }
        let generation = self.lifecycle.generation();
        self.lifecycle.mark_busy(generation)?;
        let session = self.session.clone().ok_or(AgentProcessError::NotReady)?;
        let result = session.request_with_gate(method, params, timeout, &self.stopping);
        let _ = self.lifecycle.mark_ready_again(generation);
        self.sync_terminal_signals();
        result
    }

    /// 读取外部事件；nested request 等待路径使用 session 的专用 server-request 队列。
    pub fn next_event(&mut self, timeout: Duration) -> Option<SessionEvent> {
        self.sync_terminal_signals();
        let event = self
            .event_pump
            .as_mut()
            .and_then(|pump| pump.next_event(timeout));
        self.sync_terminal_signals();
        event
    }

    /// 一次性移交唯一事件 pump；移交后 supervisor 不再提供 next_event 消费路径。
    pub fn take_event_pump(&mut self) -> Result<EventPump, AgentProcessError> {
        self.sync_terminal_signals();
        self.event_pump
            .take()
            .ok_or(AgentProcessError::InvalidState)
    }

    /// Allow the lifecycle owner to join the current writer within its shutdown
    /// deadline, making a ChildStdin cancellation observable instead of detached.
    pub fn join_writer_until(&self, deadline: Instant) -> Result<(), AgentProcessError> {
        self.session
            .as_ref()
            .ok_or(AgentProcessError::NotReady)?
            .join_writer_until(deadline)
    }

    /// 把当前可恢复 fault 推入 lifecycle backoff，并收口旧 process/session。
    pub fn schedule_restart(&mut self) -> LifecycleState {
        self.sync_terminal_signals();
        let generation = self.lifecycle.generation();
        let state = self.lifecycle.record_crash(generation);
        self.fail_process_only();
        state
    }

    /// 只允许在 backoff 到期后启动下一代，旧 generation 的 signal 不会污染它。
    pub fn restart(&mut self) -> Result<(), AgentProcessError> {
        self.sync_terminal_signals();
        if self.lifecycle.state() == LifecycleState::Backoff && !self.lifecycle.backoff_due() {
            return Err(AgentProcessError::Backoff {
                retry_after: self.lifecycle.retry_after(),
            });
        }
        *self
            .stopping
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
        self.start()
    }

    /// 先在 bounded deadline 内请求 shutdown，随后终止完整 process tree。
    pub fn shutdown(&mut self, timeout: Duration) -> Result<(), AgentProcessError> {
        let deadline = checked_deadline(timeout, MAX_SHUTDOWN_TIMEOUT)?;
        *self
            .stopping
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.sync_terminal_signals();
        let generation = self.lifecycle.generation();
        if !matches!(
            self.lifecycle.state(),
            LifecycleState::Starting | LifecycleState::Ready | LifecycleState::Busy
        ) {
            self.fail_process_only();
            let _ = self.lifecycle.mark_exited(generation);
            return Ok(());
        }
        self.lifecycle.begin_stop(generation)?;
        if let Some(session) = self.session.clone() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                let _ = session.request_with_server_request_handler(
                    "shutdown",
                    serde_json::json!({}),
                    remaining.min(self.config.shutdown_timeout),
                    |session, frame| {
                        let _ = session.respond_error(
                            frame.id(),
                            -32_020,
                            "shutting down",
                            "SHUTTING_DOWN",
                            true,
                        );
                    },
                );
            }
            let _ = session.close_until(deadline);
            session.detach_terminal_callback();
        }
        let mut exited = false;
        if let Some(process) = self.process.clone() {
            exited = process.wait_until(deadline);
            if !exited {
                let _ = process.terminate_tree_until(deadline);
                exited = process.wait_until(deadline);
            }
        }
        self.process = None;
        self.session = None;
        self.event_pump = None;
        self.sync_terminal_signals();
        let _ = self.lifecycle.mark_exited(generation);
        if exited {
            Ok(())
        } else {
            Err(AgentProcessError::ShutdownTimeout)
        }
    }

    /// 严格验证 server 的版本、实例、runtime、能力和 effective limits 后才允许 ready。
    fn check_initialize_result(&mut self, result: &Value) -> Result<(), AgentProcessError> {
        let object = result.as_object().ok_or(AgentProcessError::ProtocolFault)?;
        let major = object
            .get("protocolMajor")
            .and_then(Value::as_i64)
            .ok_or(AgentProcessError::ProtocolFault)?;
        let minor = object
            .get("protocolMinor")
            .and_then(Value::as_i64)
            .filter(|minor| (0..=i64::from(i32::MAX)).contains(minor))
            .ok_or(AgentProcessError::ProtocolFault)?;
        let local = self
            .config
            .initialize_params
            .as_object()
            .ok_or(AgentProcessError::InvalidConfig)?;
        let local_major = local
            .get("protocolMajor")
            .and_then(Value::as_i64)
            .ok_or(AgentProcessError::InvalidConfig)?;
        let local_minor = local
            .get("protocolMinor")
            .and_then(Value::as_i64)
            .ok_or(AgentProcessError::InvalidConfig)?;
        let local_minimum = local
            .get("minimumCompatibleMinor")
            .and_then(Value::as_i64)
            .ok_or(AgentProcessError::InvalidConfig)?;
        let minimum = object
            .get("minimumCompatibleMinor")
            .and_then(Value::as_i64)
            .filter(|minimum| (0..=minor).contains(minimum))
            .ok_or(AgentProcessError::ProtocolFault)?;
        codec::negotiate_version(
            local_major,
            local_minor,
            local_minimum,
            major,
            minor,
            minimum,
        )
        .map_err(|_| AgentProcessError::Incompatible)?;
        if !object
            .get("serverVersion")
            .and_then(Value::as_str)
            .is_some_and(valid_version)
        {
            return Err(AgentProcessError::ProtocolFault);
        }
        let instance = object
            .get("serverInstanceId")
            .and_then(Value::as_str)
            .ok_or(AgentProcessError::ProtocolFault)?;
        if !valid_schema_id(instance, "srv_", 101) {
            return Err(AgentProcessError::ProtocolFault);
        }
        let runtime = object
            .get("runtime")
            .and_then(Value::as_object)
            .ok_or(AgentProcessError::ProtocolFault)?;
        if runtime.get("kind").and_then(Value::as_str) != Some("native-image")
            || !bounded_string(runtime.get("agentScopeVersion"), 128)
            || !bounded_string(runtime.get("javaVersion"), 128)
        {
            return Err(AgentProcessError::ProtocolFault);
        }
        validate_capabilities(object.get("capabilities"))?;
        validate_remote_limits(object.get("limits"), &self.config.limits)?;
        if object.contains_key("workspacePolicy") {
            validate_workspace_policy(
                object.get("workspacePolicy"),
                self.config.workspace_enforcement_verified,
            )?;
        }
        self.expected_server_instance = Some(instance.to_owned());
        Ok(())
    }

    /// 将 terminal signal 映射到生命周期；只处理 current generation，拒绝旧信号污染新实例。
    fn sync_terminal_signals(&mut self) {
        let signals = {
            let mut queue = self
                .terminal_signals
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.drain(..).collect::<Vec<_>>()
        };
        for signal in signals {
            if !self.lifecycle.is_current(signal.generation) {
                continue;
            }
            match signal.reason {
                TerminalReason::Fault | TerminalReason::ProcessExited => {
                    match self.lifecycle.state() {
                        LifecycleState::Starting | LifecycleState::Ready | LifecycleState::Busy => {
                            let _ = self.lifecycle.record_crash(signal.generation);
                        }
                        LifecycleState::Stopping => {
                            let _ = self.lifecycle.mark_exited(signal.generation);
                        }
                        _ => {}
                    }
                }
                TerminalReason::Closed => {
                    if self.lifecycle.state() == LifecycleState::Stopping {
                        let _ = self.lifecycle.mark_exited(signal.generation);
                    }
                }
            }
        }
    }

    /// 失败路径必须先收口 session 与完整 process tree，再改变 lifecycle 状态。
    fn fail_generation(&mut self, generation: u64) {
        self.fail_process_only();
        let _ = self.lifecycle.mark_faulted(generation);
    }

    /// 幂等释放 process/session 资源；用于 fault、restart、shutdown 和 Drop。
    fn fail_process_only(&mut self) {
        self.event_pump = None;
        if let Some(session) = self.session.take() {
            session.close();
            session.detach_terminal_callback();
        }
        if let Some(process) = self.process.take() {
            let deadline = checked_deadline(self.config.shutdown_timeout, MAX_SHUTDOWN_TIMEOUT)
                .unwrap_or_else(|_| Instant::now());
            let _ = process.terminate_tree_until(deadline);
            let _ = process.wait_until(deadline);
        }
    }
}

impl Drop for SidecarSupervisor {
    /// Drop 也要收口完整 tree，不能只依赖 UI 调用 shutdown。
    fn drop(&mut self) {
        *self
            .stopping
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.fail_process_only();
        let generation = self.lifecycle.generation();
        let _ = self.lifecycle.mark_exited(generation);
    }
}
