// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Tauri/Wry 接线所需的最小 adapter seam。
//!
//! 当前工作面不能修改 composition root、capabilities 或依赖清单，因此
//! 不伪造一个“已经创建窗口”的实现；未接线调用明确返回 dependencyRequest，
//! 集成任务必须据此完成真实 zero-capability child Webview 验收。

use super::error::{PreviewError, PreviewErrorCode};
use super::model::{
    NavigationSource, PreviewGeneration, PreviewSessionSnapshot, PreviewUrl, PreviewWindowSpec,
    SiteDataClearAck, SiteDataClearRequest,
};

/// 真实 Tauri composition 必须满足的集成条件；纯策略模块不能自行证明它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrationRequirement {
    ZeroCapabilityChildWindow,
    NavigationHooks,
    PermissionHooks,
    SiteDataPartition,
    SiteDataClear,
}

/// 需要由共享 Tauri composition 补齐的能力类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewDependencyKind {
    ChildWindow,
    NavigationInterception,
    PermissionInterception,
    SiteDataPartition,
    SiteDataClear,
}

/// 可安全写入计划/诊断的依赖请求，不包含路径、URL 或凭据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewDependencyRequest {
    kind: PreviewDependencyKind,
    requirement: IntegrationRequirement,
}

impl PreviewDependencyRequest {
    /// 由集成层把缺少的真实 Tauri hook 记录为明确的 dependencyRequest。
    pub(crate) const fn new(
        kind: PreviewDependencyKind,
        requirement: IntegrationRequirement,
    ) -> Self {
        Self { kind, requirement }
    }

    /// 返回稳定依赖类别，供 host diagnostics 选择下一步接线。
    pub fn kind(&self) -> PreviewDependencyKind {
        self.kind
    }

    /// 返回必须由真实 Tauri ACL/window hook 满足的集成条件。
    pub fn requirement(&self) -> IntegrationRequirement {
        self.requirement
    }
}

/// 只允许同 crate trusted Tauri composition 实现的 host seam。
#[allow(private_bounds)]
pub trait PreviewHostAdapter: sealed::Sealed + Send + Sync {
    /// 创建独立 child Webview；实现必须使用 `PreviewWindowSpec` 的空 capability 集。
    fn open_window(
        &self,
        _snapshot: &PreviewSessionSnapshot,
        _window: &PreviewWindowSpec,
    ) -> Result<(), PreviewDependencyRequest> {
        Err(PreviewDependencyRequest::new(
            PreviewDependencyKind::ChildWindow,
            IntegrationRequirement::ZeroCapabilityChildWindow,
        ))
    }

    /// 关闭窗口并释放所有 platform callback/listener。
    fn close_window(&self, _window: &PreviewWindowSpec) -> Result<(), PreviewDependencyRequest> {
        Err(PreviewDependencyRequest::new(
            PreviewDependencyKind::ChildWindow,
            IntegrationRequirement::ZeroCapabilityChildWindow,
        ))
    }

    /// 由 platform hook 消费已由 Session policy 先验的 URL/generation 请求。
    fn intercept_navigation(
        &self,
        _request: &PreviewNavigationRequest,
        _window: &PreviewWindowSpec,
    ) -> Result<(), PreviewDependencyRequest> {
        Err(PreviewDependencyRequest::new(
            PreviewDependencyKind::NavigationInterception,
            IntegrationRequirement::NavigationHooks,
        ))
    }

    /// 由 platform hook 默认拒绝 download/permission/file chooser 等请求。
    fn intercept_permissions(
        &self,
        _window: &PreviewWindowSpec,
    ) -> Result<(), PreviewDependencyRequest> {
        Err(PreviewDependencyRequest::new(
            PreviewDependencyKind::PermissionInterception,
            IntegrationRequirement::PermissionHooks,
        ))
    }

    /// 清除 partition 站点数据；只有 opaque ACK 才能进入 Completed。
    fn clear_site_data(
        &self,
        _request: &SiteDataClearRequest,
    ) -> Result<SiteDataClearAck, PreviewDependencyRequest> {
        Err(PreviewDependencyRequest::new(
            PreviewDependencyKind::SiteDataClear,
            IntegrationRequirement::SiteDataClear,
        ))
    }
}

/// 已由 Session policy 校验的 adapter 输入，禁止 trusted adapter 接受 raw URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewNavigationRequest {
    session_id: super::model::PreviewId,
    candidate_url: PreviewUrl,
    kind: NavigationSource,
    from_generation: PreviewGeneration,
    to_generation: PreviewGeneration,
}

impl PreviewNavigationRequest {
    /// 由 Session 创建并绑定 operation identity，普通调用方不能构造。
    pub(crate) fn new(
        session_id: super::model::PreviewId,
        candidate_url: PreviewUrl,
        kind: NavigationSource,
        from_generation: PreviewGeneration,
        to_generation: PreviewGeneration,
    ) -> Self {
        Self {
            session_id,
            candidate_url,
            kind,
            from_generation,
            to_generation,
        }
    }

    /// adapter 只能读取已经规范化的 candidate URL。
    pub fn candidate_url(&self) -> &PreviewUrl {
        &self.candidate_url
    }

    /// adapter 只能读取 Session 选择的 navigation kind。
    pub fn kind(&self) -> NavigationSource {
        self.kind
    }

    /// adapter 用 from generation 丢弃页面旧 callback。
    pub fn from_generation(&self) -> PreviewGeneration {
        self.from_generation
    }

    /// adapter 用 to generation 绑定新 operation 的 callback。
    pub fn to_generation(&self) -> PreviewGeneration {
        self.to_generation
    }

    /// adapter 不拥有 session，只能读取 owner identity 做日志关联。
    pub fn session_id(&self) -> super::model::PreviewId {
        self.session_id
    }
}

/// 让普通 crate 外部无法伪造 host adapter；新增实现必须在可信 composition 内显式审计。
pub(crate) mod sealed {
    pub(crate) trait Sealed {}
}

/// 在 shared wiring 尚未接入前的显式 fail-closed adapter。
#[derive(Debug, Default, Clone, Copy)]
pub struct UnwiredPreviewAdapter;

impl sealed::Sealed for UnwiredPreviewAdapter {}

impl PreviewHostAdapter for UnwiredPreviewAdapter {}

/// 将 dependencyRequest 映射为稳定 UI 错误，避免调用方把未接线当成功。
pub(crate) fn dependency_error(_request: PreviewDependencyRequest) -> PreviewError {
    PreviewError::new(PreviewErrorCode::DependencyRequest)
}
