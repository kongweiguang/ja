// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

//! Java AgentScope sidecar 的 Rust host 边界。
//!
//! `mod agent_process;` 可由 Tauri composition root 在后续集成；本模块自身不依赖
//! Tauri state，因而协议、并发和生命周期可以在独立 integration test 中先验收。

pub mod codec;
pub mod error;
mod handshake;
pub mod lifecycle;
pub mod pending;
mod process_tree;
pub mod session;
pub mod supervisor;

pub use codec::{CodecError, Limits, RpcFrame};
pub use error::AgentProcessError;
pub use lifecycle::{LifecycleMachine, LifecycleState, RestartPolicy};
pub use session::{EventPump, Session, SessionEvent, SupervisorEventPump};
pub use supervisor::{AgentClient, SidecarConfig, SidecarSupervisor};
