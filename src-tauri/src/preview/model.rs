// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Preview 的不透明身份、事件和 child-window 数据模型。
//!
//! 这些类型不携带原生 WebView 句柄，便于在没有 Tauri runtime 的情况下
//! 验证策略；真正的 platform adapter 只能消费不可变的窗口规格。

use super::error::{PreviewError, PreviewErrorCode};
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// 用户可观察的 Preview session 身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PreviewId(Uuid);

impl PreviewId {
    /// 使用随机身份，避免页面或前端能够预测另一个 session 的 owner。
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 为 Tauri label 生成不含 URL/路径的稳定形状。
    pub(crate) fn label(self) -> String {
        format!("preview_{}", self.0.simple())
    }
}

impl Display for PreviewId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// 每次新导航后的回调隔离代数。
pub type PreviewGeneration = u64;

/// 只允许由 PreviewPolicy 构造的规范化 HTTP(S) URL。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PreviewUrl(String);

impl PreviewUrl {
    /// 该构造器只在策略完成所有 scheme/host/userinfo/size 校验后调用。
    pub(crate) fn from_normalized(value: String) -> Self {
        Self(value)
    }

    /// 提供给窗口 adapter 的规范化地址。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for PreviewUrl {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PreviewUrl {
    /// 反序列化仍走默认 policy，防止外部 DTO 伪造 data/file/javascript URL。
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        super::policy::PreviewPolicy::new()
            .and_then(|policy| policy.validate_url(&raw))
            .map_err(serde::de::Error::custom)
    }
}

/// Preview session 的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSessionStatus {
    Open,
    Closed,
}

/// 导航来源用于区分用户动作、重定向和受控新窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationSource {
    User,
    Redirect,
    NewWindow,
}

/// WebView 受权限询问时的稳定资源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewPermission {
    FileChooser,
    Download,
    Media,
    Geolocation,
    Notification,
    ClipboardRead,
    ClipboardWrite,
    Camera,
    Microphone,
    Popup,
    DragDrop,
}

/// 默认全部拒绝；如果未来需要允许某一项，必须在 host adapter 层显式审计。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Deny,
}

/// 受控 child window 的站点数据 partition。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct SiteDataPartition(String);

impl SiteDataPartition {
    /// 每个 session 使用独立随机 partition，避免不同预览共享 cookie/cache。
    pub(crate) fn new(id: PreviewId) -> Self {
        Self(format!("ja-preview-{}", id.0.simple()))
    }

    /// 仅把不透明 partition key 交给 platform adapter，不暴露本机目录。
    pub fn key(&self) -> &str {
        &self.0
    }
}

/// 主机需要实现的 zero-capability window 描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewWindowSpec {
    label: String,
    url: PreviewUrl,
    partition: SiteDataPartition,
}

impl PreviewWindowSpec {
    /// 创建不可获得主窗口能力的规格；字段私有避免调用方打开权限开关。
    pub(crate) fn new(id: PreviewId, url: PreviewUrl, partition: SiteDataPartition) -> Self {
        Self {
            label: id.label(),
            url,
            partition,
        }
    }

    /// 返回 child window label，供 Tauri adapter 做单窗口生命周期管理。
    pub fn label(&self) -> &str {
        &self.label
    }

    /// 返回已经通过策略校验的首个 URL。
    pub fn url(&self) -> &PreviewUrl {
        &self.url
    }

    /// 返回独立站点数据 partition。
    pub fn partition(&self) -> &SiteDataPartition {
        &self.partition
    }
}

/// 单个 Preview 事件；sequence 在 session 内单调递增。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewEvent {
    pub session_id: PreviewId,
    pub generation: PreviewGeneration,
    pub sequence: u64,
    pub kind: PreviewEventKind,
}

/// 受界面消费的 bounded 事件种类。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum PreviewEventKind {
    Opened {
        url: PreviewUrl,
    },
    NavigationCommitted {
        source: NavigationSource,
        url: PreviewUrl,
    },
    NewWindowOpened {
        url: PreviewUrl,
    },
    NavigationBlocked,
    NewWindowBlocked,
    TitleChanged {
        title: String,
    },
    LoadFailed {
        message: String,
    },
    PermissionDenied {
        permission: PreviewPermission,
    },
    DownloadBlocked,
    CertificateRejected,
    PopupBlocked,
    DragDropBlocked,
    SiteDataClearRequested {
        request_id: Uuid,
        deadline_unix_millis: u64,
    },
    SiteDataCleared,
    SiteDataClearFailed,
    EventsDropped {
        count: u64,
    },
    Closed,
}

/// 外部 platform callback 的最小白名单；没有脚本/DOM/凭据输入字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewCallback {
    Navigation { url: String },
    Title { title: String },
    LoadError { message: String },
    Permission { permission: PreviewPermission },
}

/// 会话创建返回的快照与 zero-capability 窗口规格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewOpenResult {
    pub snapshot: PreviewSessionSnapshot,
    pub window: PreviewWindowSpec,
}

/// 受控 session 当前状态；不包含 WebView 句柄或原始页面内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewSessionSnapshot {
    pub id: PreviewId,
    pub generation: PreviewGeneration,
    pub status: PreviewSessionStatus,
    pub url: PreviewUrl,
    pub title: String,
    pub partition: SiteDataPartition,
    pub window: PreviewWindowSpec,
    pub dropped_events: u64,
}

/// 新窗口/导航经过策略后返回的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PreviewNavigationResult {
    Committed(PreviewSessionSnapshot),
    Opened {
        parent: PreviewSessionSnapshot,
        child: Box<PreviewOpenResult>,
    },
}

/// 站点数据清理的实际执行结果，由 platform adapter 回填。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ClearSiteDataOutcome {
    Completed,
    Failed,
}

/// 站点数据清理请求；token 字段私有以防止普通调用方伪造 ACK。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteDataClearRequest {
    request_id: Uuid,
    session_id: PreviewId,
    generation: PreviewGeneration,
    partition: SiteDataPartition,
    deadline: Instant,
}

impl SiteDataClearRequest {
    /// 只有 Session 在持有当前 generation 和 partition 时创建 opaque token。
    pub(crate) fn new(
        request_id: Uuid,
        session_id: PreviewId,
        generation: PreviewGeneration,
        partition: SiteDataPartition,
        deadline: Instant,
    ) -> Self {
        Self {
            request_id,
            session_id,
            generation,
            partition,
            deadline,
        }
    }

    /// 只暴露不透明 request id，便于 adapter 记录而不能重建完整 token。
    pub(crate) fn request_id(&self) -> Uuid {
        self.request_id
    }

    /// adapter 需要绑定 session owner，但不能修改该身份。
    pub(crate) fn session_id(&self) -> PreviewId {
        self.session_id
    }

    /// adapter 需要把 ACK 绑定到创建时的 generation。
    pub(crate) fn generation(&self) -> PreviewGeneration {
        self.generation
    }

    /// adapter 只能读取不透明 partition，不能选择另一个 session 的数据。
    pub(crate) fn partition(&self) -> &SiteDataPartition {
        &self.partition
    }

    /// 给内部 watchdog 使用绝对 monotonic deadline，避免相对 timeout 被重置。
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
}

/// 只有同 crate 可信 adapter 才能构造的清理 ACK。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteDataClearAck {
    request_id: Uuid,
    session_id: PreviewId,
    generation: PreviewGeneration,
    partition: SiteDataPartition,
    deadline: Instant,
    outcome: ClearSiteDataOutcome,
}

#[allow(dead_code)]
impl SiteDataClearAck {
    /// 真实 adapter 在成功清理目标 partition 后生成绑定 ACK。
    pub(crate) fn completed(request: &SiteDataClearRequest) -> Self {
        Self::from_request(request, ClearSiteDataOutcome::Completed)
    }

    /// 真实 adapter 在清理失败后生成 terminal failure ACK。
    pub(crate) fn failed(request: &SiteDataClearRequest) -> Self {
        Self::from_request(request, ClearSiteDataOutcome::Failed)
    }

    /// 仅复制 opaque request identity，防止 adapter 自行指定 session/partition。
    fn from_request(request: &SiteDataClearRequest, outcome: ClearSiteDataOutcome) -> Self {
        Self {
            request_id: request.request_id,
            session_id: request.session_id,
            generation: request.generation,
            partition: request.partition.clone(),
            deadline: request.deadline,
            outcome,
        }
    }

    /// Session 只读取 ACK identity 做完整绑定校验。
    pub(crate) fn request_id(&self) -> Uuid {
        self.request_id
    }

    /// Session 只读取 ACK owner identity，不能从外部修改。
    pub(crate) fn session_id(&self) -> PreviewId {
        self.session_id
    }

    /// Session 只读取 ACK generation，不能从外部修改。
    pub(crate) fn generation(&self) -> PreviewGeneration {
        self.generation
    }

    /// Session 只读取 ACK partition，不能从外部修改。
    pub(crate) fn partition(&self) -> &SiteDataPartition {
        &self.partition
    }

    /// Session 用原始 monotonic deadline 防止 timeout 被 ACK 重置。
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Session 读取 trusted adapter 的 terminal outcome。
    pub(crate) fn outcome(&self) -> ClearSiteDataOutcome {
        self.outcome
    }
}

/// 绑定到所有 session 的资源上限，防止恶意页面拖垮主进程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewLimits {
    pub max_sessions: usize,
    pub max_tombstones: usize,
    pub max_url_bytes: usize,
    pub max_title_bytes: usize,
    pub max_error_bytes: usize,
    pub max_event_count: usize,
    pub max_event_payload_bytes: usize,
    pub max_event_queue_bytes: usize,
    pub max_terminal_event_count: usize,
    pub max_pending_clear_requests: usize,
    pub site_data_clear_timeout: Duration,
}

impl Default for PreviewLimits {
    /// 默认值让常规页面可用，同时给事件/字符串/窗口数量明确上限。
    fn default() -> Self {
        Self {
            max_sessions: 8,
            max_tombstones: 64,
            max_url_bytes: 8 * 1024,
            max_title_bytes: 1024,
            max_error_bytes: 4 * 1024,
            max_event_count: 512,
            max_event_payload_bytes: 64 * 1024,
            max_event_queue_bytes: 2 * 1024 * 1024,
            max_terminal_event_count: 32,
            max_pending_clear_requests: 4,
            site_data_clear_timeout: Duration::from_secs(30),
        }
    }
}

impl PreviewLimits {
    /// 启动 registry 前验证所有关系，避免队列在运行时才发现不可能的预算。
    pub(crate) fn validate(self) -> Result<Self, PreviewError> {
        let valid = self.max_sessions > 0
            && self.max_sessions <= 64
            && self.max_tombstones > 0
            && self.max_tombstones <= 1024
            && self.max_url_bytes >= 64
            && self.max_url_bytes <= 64 * 1024
            && self.max_title_bytes > 0
            && self.max_title_bytes <= 64 * 1024
            && self.max_error_bytes > 0
            && self.max_error_bytes <= 256 * 1024
            && self.max_event_count > 0
            && self.max_event_count <= 65_536
            && self.max_event_payload_bytes >= 128
            && self.max_event_payload_bytes <= 4 * 1024 * 1024
            && self.max_event_queue_bytes >= self.max_event_payload_bytes
            && self.max_event_queue_bytes <= 64 * 1024 * 1024
            && self.max_terminal_event_count > 0
            && self.max_terminal_event_count <= 128
            && self.max_pending_clear_requests > 0
            && self.max_pending_clear_requests <= 64
            && self.max_terminal_event_count > self.max_pending_clear_requests
            && !self.site_data_clear_timeout.is_zero()
            && self.site_data_clear_timeout <= Duration::from_secs(120);
        if valid {
            Ok(self)
        } else {
            Err(PreviewError::new(PreviewErrorCode::InvalidConfig))
        }
    }
}

impl PreviewEvent {
    /// 计算真实 JSON event 大小，确保 queue budget 不是字符串长度的乐观估计。
    pub(crate) fn encoded_bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }
}
