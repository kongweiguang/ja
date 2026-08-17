// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! 隔离的用户网页预览边界。
//!
//! 该模块只负责 URL/导航/事件/会话的可验证策略，不把远程页面载入主
//! WebView，也不假装已经完成 Tauri composition。真正的 child Webview
//! 接线必须消费 [`PreviewWindowSpec`]，并以零 capability 创建窗口。

mod adapter;
mod error;
mod model;
mod policy;
mod session;

pub use adapter::{
    IntegrationRequirement, PreviewDependencyKind, PreviewDependencyRequest, PreviewHostAdapter,
    PreviewNavigationRequest, UnwiredPreviewAdapter,
};
pub use error::{PreviewError, PreviewErrorCode};
pub use model::{
    NavigationSource, PermissionDecision, PreviewCallback, PreviewEvent, PreviewEventKind,
    PreviewGeneration, PreviewId, PreviewLimits, PreviewNavigationResult, PreviewOpenResult,
    PreviewPermission, PreviewSessionSnapshot, PreviewSessionStatus, PreviewUrl, PreviewWindowSpec,
    SiteDataClearAck, SiteDataClearRequest, SiteDataPartition,
};
pub use policy::{
    CertificateErrorPolicy, DragDropPolicy, NewWindowBehavior, PreviewNavigationDecision,
    PreviewPolicy,
};
pub use session::{PreviewManager, PreviewSession, PreviewSessionRegistry};

#[cfg(test)]
mod tests;
