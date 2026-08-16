// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Cloneable client handle for one sidecar session generation.

use crate::agent_process::codec::RpcFrame;
use crate::agent_process::error::AgentProcessError;
use crate::agent_process::session::Session;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

/// 可复制的受控 client handle；它只绑定一个 session generation，不持有 supervisor 锁。
#[derive(Clone)]
pub struct AgentClient {
    pub(super) session: Session,
    pub(super) generation: u64,
    pub(super) stopping: Arc<Mutex<bool>>,
}

impl AgentClient {
    /// 返回 handle 绑定的 generation，供上层拒绝跨重启复用的旧请求。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 并发向同一 sidecar 发起 request，pending 上限由 session 统一执行。
    pub fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<RpcFrame, AgentProcessError> {
        self.session
            .request_with_gate(method, params, timeout, &self.stopping)
    }

    /// 发送 notification，不要求 caller 持有 supervisor 的可变借用。
    pub fn notify(&self, method: &str, params: Value) -> Result<(), AgentProcessError> {
        self.session
            .notify_with_gate(method, params, &self.stopping)
    }

    /// 回应 sidecar 发起的 server request，保持 response 的一次性 tombstone 约束。
    pub fn respond_result(&self, id: &str, result: Value) -> Result<(), AgentProcessError> {
        self.session.respond_result(id, result)
    }

    /// 发送脱敏 server-request error，并由 session 统一处理 writer fault。
    pub fn respond_error(
        &self,
        id: &str,
        code: i64,
        message: &str,
        ja_code: &str,
        retryable: bool,
    ) -> Result<(), AgentProcessError> {
        self.session
            .respond_error(id, code, message, ja_code, retryable)
    }
}
