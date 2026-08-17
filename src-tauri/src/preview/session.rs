// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Preview session registry、代数和 bounded 事件生命周期。
//!
//! 该层不持有 WebView 句柄；它只保证晚到 callback 不会写入新页面、关闭
//! 幂等且事件/站点数据请求有界。真实窗口由 adapter 在外层拥有和清理。

use super::adapter::{PreviewHostAdapter, PreviewNavigationRequest, dependency_error};
use super::error::{PreviewError, PreviewErrorCode};
use super::model::{
    ClearSiteDataOutcome, NavigationSource, PermissionDecision, PreviewCallback, PreviewEvent,
    PreviewEventKind, PreviewGeneration, PreviewId, PreviewLimits, PreviewNavigationResult,
    PreviewOpenResult, PreviewPermission, PreviewSessionSnapshot, PreviewSessionStatus, PreviewUrl,
    PreviewWindowSpec, SiteDataClearAck, SiteDataClearRequest, SiteDataPartition,
};
use super::policy::{PreviewNavigationDecision, PreviewPolicy};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// 可跨 Tauri command 复制的 Preview manager。
#[derive(Clone)]
pub struct PreviewManager {
    policy: Arc<PreviewPolicy>,
    state: Arc<Mutex<RegistryState>>,
}

/// 兼容计划文档中的 registry 命名。
pub type PreviewSessionRegistry = PreviewManager;

/// 面向调用方的 session 命名；manager 本身持有多个 bounded session。
pub type PreviewSession = PreviewManager;

struct RegistryState {
    sessions: HashMap<PreviewId, SessionState>,
    closed_order: VecDeque<PreviewId>,
}

struct SessionState {
    id: PreviewId,
    generation: PreviewGeneration,
    status: PreviewSessionStatus,
    url: PreviewUrl,
    title: String,
    partition: SiteDataPartition,
    events: VecDeque<PreviewEvent>,
    terminal_events: VecDeque<PreviewEvent>,
    event_bytes: usize,
    next_sequence: u64,
    dropped_events: u64,
    pending_clear: HashMap<Uuid, SiteDataClearRequest>,
}

impl PreviewManager {
    /// 建立带资源预算的 registry；策略和状态均由 manager 共享所有权。
    pub fn new(policy: PreviewPolicy) -> Result<Self, PreviewError> {
        policy.limits().validate()?;
        Ok(Self {
            policy: Arc::new(policy),
            state: Arc::new(Mutex::new(RegistryState {
                sessions: HashMap::new(),
                closed_order: VecDeque::new(),
            })),
        })
    }

    /// 使用默认 URL/事件策略创建 manager。
    pub fn default_manager() -> Result<Self, PreviewError> {
        Self::new(PreviewPolicy::new()?)
    }

    /// 打开一个独立 session，并返回给 Tauri adapter 的零 capability 规格。
    pub fn open(&self, raw_url: &str) -> Result<PreviewOpenResult, PreviewError> {
        let url = self.policy.validate_url(raw_url)?;
        let mut state = self.lock_state()?;
        self.open_locked(&mut state, url)
    }

    /// 先由 Session policy 生成带 from/to generation 的可信 adapter 输入。
    pub fn navigation_request(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        source: NavigationSource,
        raw_url: &str,
    ) -> Result<PreviewNavigationRequest, PreviewError> {
        let decision = self.policy.navigation(source, raw_url)?;
        let state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let url = match decision {
            PreviewNavigationDecision::Allow { url }
            | PreviewNavigationDecision::OpenControlled { url } => url,
            PreviewNavigationDecision::Reject { code } => return Err(PreviewError::new(code)),
        };
        Ok(PreviewNavigationRequest::new(
            id,
            url,
            source,
            generation,
            next_generation(generation)?,
        ))
    }

    /// 真实 host 只能在 Session policy 先验后注册 navigation hook，未接线时 fail closed。
    pub fn navigate_with_adapter<A: PreviewHostAdapter>(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        source: NavigationSource,
        raw_url: &str,
        adapter: &A,
    ) -> Result<PreviewNavigationResult, PreviewError> {
        let request = self.navigation_request(id, generation, source, raw_url)?;
        let snapshot = self.snapshot(id)?;
        adapter
            .intercept_navigation(&request, &snapshot.window)
            .map_err(dependency_error)?;
        self.navigate(id, generation, source, raw_url)
    }

    /// 对现有 session 的用户导航/重定向/新窗口统一重验。
    pub fn navigate(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        source: NavigationSource,
        raw_url: &str,
    ) -> Result<PreviewNavigationResult, PreviewError> {
        let decision = self.policy.navigation(source, raw_url)?;
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        match decision {
            PreviewNavigationDecision::Allow { url } => {
                let session = state
                    .sessions
                    .get_mut(&id)
                    .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
                session.generation = next_generation(session.generation)?;
                session.url = url.clone();
                session.title.clear();
                // A clear request belongs to the old document; dropping it prevents a
                // late platform completion from claiming the newly navigated page.
                session.pending_clear.clear();
                Self::push_event(
                    session,
                    PreviewEventKind::NavigationCommitted { source, url },
                    self.policy.limits(),
                )?;
                Ok(PreviewNavigationResult::Committed(Self::snapshot_state(
                    session,
                )))
            }
            PreviewNavigationDecision::OpenControlled { url } => {
                let parent_generation = next_generation(generation)?;
                let child = self.open_locked(&mut state, url.clone())?;
                let parent = state
                    .sessions
                    .get_mut(&id)
                    .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
                parent.generation = parent_generation;
                parent.pending_clear.clear();
                if let Err(error) = Self::push_event(
                    parent,
                    PreviewEventKind::NewWindowOpened { url },
                    self.policy.limits(),
                ) {
                    state.sessions.remove(&child.snapshot.id);
                    return Err(error);
                }
                let parent_snapshot = Self::snapshot_state(parent);
                Ok(PreviewNavigationResult::Opened {
                    parent: parent_snapshot,
                    child: Box::new(child),
                })
            }
            PreviewNavigationDecision::Reject { code } => {
                let session = state
                    .sessions
                    .get_mut(&id)
                    .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
                let event = match code {
                    PreviewErrorCode::NewWindowBlocked => PreviewEventKind::NewWindowBlocked,
                    _ => PreviewEventKind::NavigationBlocked,
                };
                Self::push_event(session, event, self.policy.limits())?;
                Err(PreviewError::new(code))
            }
        }
    }

    /// 接受 platform 的 committed-navigation callback，并再次校验 URL。
    pub fn callback_navigation(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        raw_url: &str,
    ) -> Result<PreviewEvent, PreviewError> {
        let url = self.policy.validate_url(raw_url)?;
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        session.generation = next_generation(session.generation)?;
        session.url = url.clone();
        session.title.clear();
        session.pending_clear.clear();
        Self::push_event(
            session,
            PreviewEventKind::NavigationCommitted {
                source: NavigationSource::Redirect,
                url,
            },
            self.policy.limits(),
        )
    }

    /// 接受 bounded title callback；过长标题截断到完整 UTF-8 边界。
    pub fn callback_title(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        title: &str,
    ) -> Result<PreviewEvent, PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        let title = truncate_utf8(title, self.policy.limits().max_title_bytes);
        session.title = title.clone();
        Self::push_event(
            session,
            PreviewEventKind::TitleChanged { title },
            self.policy.limits(),
        )
    }

    /// 接受 bounded load error callback；底层错误原文永不进入稳定错误对象。
    pub fn callback_load_error(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        message: &str,
    ) -> Result<PreviewEvent, PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        let message = truncate_utf8(message, self.policy.limits().max_error_bytes);
        Self::push_event(
            session,
            PreviewEventKind::LoadFailed { message },
            self.policy.limits(),
        )
    }

    /// 接受任意 callback 的最小白名单，调用方无需自行判断 generation。
    pub fn callback(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        callback: PreviewCallback,
    ) -> Result<PreviewEvent, PreviewError> {
        match callback {
            PreviewCallback::Navigation { url } => self.callback_navigation(id, generation, &url),
            PreviewCallback::Title { title } => self.callback_title(id, generation, &title),
            PreviewCallback::LoadError { message } => {
                self.callback_load_error(id, generation, &message)
            }
            PreviewCallback::Permission { permission } => {
                self.permission(id, generation, permission)
            }
        }
    }

    /// 所有 WebView 权限 callback 默认拒绝，并留下可见审计事件。
    pub fn permission(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        permission: PreviewPermission,
    ) -> Result<PreviewEvent, PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        Self::push_event(
            session,
            PreviewEventKind::PermissionDenied { permission },
            self.policy.limits(),
        )
    }

    /// 下载请求默认拒绝，避免 adapter 意外落盘或打开系统下载目录。
    pub fn download(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
    ) -> Result<PermissionDecision, PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        Self::push_event(
            session,
            PreviewEventKind::DownloadBlocked,
            self.policy.limits(),
        )?;
        Err(PreviewError::new(PreviewErrorCode::DownloadBlocked))
    }

    /// 证书错误默认拒绝，并且只接受当前 generation 的 callback。
    pub fn certificate_error(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
    ) -> Result<(), PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        Self::push_event(
            session,
            PreviewEventKind::CertificateRejected,
            self.policy.limits(),
        )?;
        Err(PreviewError::new(PreviewErrorCode::CertificateRejected))
    }

    /// popup 被阻止；受控新窗口必须重新进入 `navigate(NewWindow, ...)`。
    pub fn popup(&self, id: PreviewId, generation: PreviewGeneration) -> Result<(), PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        Self::push_event(
            session,
            PreviewEventKind::PopupBlocked,
            self.policy.limits(),
        )?;
        Err(PreviewError::new(PreviewErrorCode::PopupBlocked))
    }

    /// 拖放被阻止；页面不能获得本机路径或文件内容。
    pub fn drag_drop(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
    ) -> Result<(), PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        Self::push_event(
            session,
            PreviewEventKind::DragDropBlocked,
            self.policy.limits(),
        )?;
        Err(PreviewError::new(PreviewErrorCode::DragDropBlocked))
    }

    /// 关闭 session 幂等化，并保留有界 tombstone 让旧 callback 明确失败。
    pub fn close(&self, id: PreviewId) -> Result<PreviewSessionSnapshot, PreviewError> {
        let mut state = self.lock_state()?;
        self.close_locked(&mut state, id)
    }

    /// 关闭 callback 必须携带当前 generation，防止旧页面关闭新页面窗口。
    pub fn callback_close(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
    ) -> Result<PreviewSessionSnapshot, PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        self.close_locked(&mut state, id)
    }

    /// 在同一锁内完成幂等关闭，使用户动作与 callback 共享终态逻辑。
    fn close_locked(
        &self,
        state: &mut RegistryState,
        id: PreviewId,
    ) -> Result<PreviewSessionSnapshot, PreviewError> {
        let already_closed = state
            .sessions
            .get(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?
            .status
            == PreviewSessionStatus::Closed;
        if already_closed {
            return state
                .sessions
                .get(&id)
                .map(Self::snapshot_state)
                .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound));
        }
        let closed_snapshot = {
            let session = state
                .sessions
                .get_mut(&id)
                .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
            session.status = PreviewSessionStatus::Closed;
            session.pending_clear.clear();
            Self::push_event(session, PreviewEventKind::Closed, self.policy.limits())?;
            Self::snapshot_state(session)
        };
        state.closed_order.push_back(id);
        self.prune_closed(state);
        Ok(closed_snapshot)
    }

    /// 以顺序消费事件，防止 UI reload 后丢掉未处理的 bounded callback。
    pub fn drain_events(
        &self,
        id: PreviewId,
        max_events: usize,
    ) -> Result<Vec<PreviewEvent>, PreviewError> {
        let mut state = self.lock_state()?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        let count = max_events.min(self.policy.limits().max_event_count);
        let mut drained = Vec::with_capacity(
            count.min(
                session
                    .events
                    .len()
                    .saturating_add(session.terminal_events.len()),
            ),
        );
        for _ in 0..count {
            let Some(event) = pop_next_event(session) else {
                break;
            };
            session.event_bytes = session.event_bytes.saturating_sub(event.encoded_bytes());
            drained.push(event);
        }
        Ok(drained)
    }

    /// 为当前 partition 创建 bounded clear request；实际清理由 platform adapter 执行。
    pub fn request_site_data_clear(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
    ) -> Result<SiteDataClearRequest, PreviewError> {
        let mut state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        if session.pending_clear.len() >= self.policy.limits().max_pending_clear_requests {
            return Err(PreviewError::new(PreviewErrorCode::SiteDataClearPending));
        }
        let deadline = Instant::now()
            .checked_add(self.policy.limits().site_data_clear_timeout)
            .ok_or(PreviewError::new(
                PreviewErrorCode::InternalStateUnavailable,
            ))?;
        let request = SiteDataClearRequest::new(
            Uuid::new_v4(),
            id,
            generation,
            session.partition.clone(),
            deadline,
        );
        session
            .pending_clear
            .insert(request.request_id(), request.clone());
        if let Err(error) = Self::push_event(
            session,
            PreviewEventKind::SiteDataClearRequested {
                request_id: request.request_id(),
                deadline_unix_millis: deadline_unix_millis(deadline),
            },
            self.policy.limits(),
        ) {
            session.pending_clear.remove(&request.request_id());
            return Err(error);
        }
        Ok(request)
    }

    /// 只有 trusted adapter 生成的 opaque ACK 才能完成清理，重复或晚到回调 fail closed。
    pub fn complete_site_data_clear(
        &self,
        ack: SiteDataClearAck,
        now: Instant,
    ) -> Result<PreviewEvent, PreviewError> {
        let mut state = self.lock_state()?;
        let session = state
            .sessions
            .get_mut(&ack.session_id())
            .ok_or(PreviewError::new(PreviewErrorCode::SiteDataClearStale))?;
        let Some(owned) = session.pending_clear.get(&ack.request_id()) else {
            return Err(PreviewError::new(PreviewErrorCode::SiteDataClearStale));
        };
        if owned.generation() != ack.generation()
            || owned.partition() != ack.partition()
            || owned.deadline() != ack.deadline()
            || session.generation != ack.generation()
        {
            session.pending_clear.remove(&ack.request_id());
            return Err(PreviewError::new(PreviewErrorCode::SiteDataClearStale));
        }
        if session.status == PreviewSessionStatus::Closed {
            return Err(PreviewError::new(PreviewErrorCode::SessionClosed));
        }
        session.pending_clear.remove(&ack.request_id());
        if now >= ack.deadline() {
            Self::push_event(
                session,
                PreviewEventKind::SiteDataClearFailed,
                self.policy.limits(),
            )?;
            return Err(PreviewError::new(PreviewErrorCode::SiteDataClearExpired));
        }
        let kind = match ack.outcome() {
            ClearSiteDataOutcome::Completed => PreviewEventKind::SiteDataCleared,
            ClearSiteDataOutcome::Failed => PreviewEventKind::SiteDataClearFailed,
        };
        Self::push_event(session, kind, self.policy.limits())
    }

    /// 真实 adapter 不可用时立即终止 pending operation，绝不伪造 Completed。
    pub fn clear_site_data_with_adapter<A: PreviewHostAdapter>(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        adapter: &A,
    ) -> Result<PreviewEvent, PreviewError> {
        let request = self.request_site_data_clear(id, generation)?;
        let ack = match adapter.clear_site_data(&request) {
            Ok(ack) => ack,
            Err(_) => {
                self.fail_pending_site_data(&request)?;
                return Err(PreviewError::new(PreviewErrorCode::DependencyRequest));
            }
        };
        self.complete_site_data_clear(ack, Instant::now())
    }

    /// watchdog 使用同一绝对 deadline 结束超时请求并写入 terminal lane。
    pub fn expire_site_data_clear(&self, now: Instant) -> Result<usize, PreviewError> {
        let mut state = self.lock_state()?;
        let limits = self.policy.limits();
        let mut expired = 0;
        for session in state.sessions.values_mut() {
            let ids = session
                .pending_clear
                .iter()
                .filter_map(|(id, request)| (request.deadline() <= now).then_some(*id))
                .collect::<Vec<_>>();
            for id in ids {
                session.pending_clear.remove(&id);
                Self::push_event(session, PreviewEventKind::SiteDataClearFailed, limits)?;
                expired += 1;
            }
        }
        Ok(expired)
    }

    /// 将没有 platform ACK 的请求转成 terminal failure，防止 pending 永久占位。
    fn fail_pending_site_data(&self, request: &SiteDataClearRequest) -> Result<(), PreviewError> {
        let mut state = self.lock_state()?;
        let session = state
            .sessions
            .get_mut(&request.session_id())
            .ok_or(PreviewError::new(PreviewErrorCode::SiteDataClearStale))?;
        if session
            .pending_clear
            .remove(&request.request_id())
            .is_none()
        {
            return Err(PreviewError::new(PreviewErrorCode::SiteDataClearStale));
        }
        Self::push_event(
            session,
            PreviewEventKind::SiteDataClearFailed,
            self.policy.limits(),
        )?;
        Ok(())
    }

    /// 读取最新快照，供窗口重连或 late subscriber 先恢复状态。
    pub fn snapshot(&self, id: PreviewId) -> Result<PreviewSessionSnapshot, PreviewError> {
        let state = self.lock_state()?;
        state
            .sessions
            .get(&id)
            .map(Self::snapshot_state)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))
    }

    /// 返回当前仍存活的 session 数，不统计 tombstone。
    pub fn active_count(&self) -> Result<usize, PreviewError> {
        let state = self.lock_state()?;
        Ok(state
            .sessions
            .values()
            .filter(|session| session.status == PreviewSessionStatus::Open)
            .count())
    }

    /// 统一映射任何外部 callback，确保锁 poisoned 不会被当作可用状态继续运行。
    fn lock_state(&self) -> Result<MutexGuard<'_, RegistryState>, PreviewError> {
        self.state
            .lock()
            .map_err(|_| PreviewError::new(PreviewErrorCode::InternalStateUnavailable))
    }

    /// 在同一锁内创建 session，避免 session limit 与插入之间出现竞态。
    fn open_locked(
        &self,
        state: &mut RegistryState,
        url: PreviewUrl,
    ) -> Result<PreviewOpenResult, PreviewError> {
        let active_count = state
            .sessions
            .values()
            .filter(|session| session.status == PreviewSessionStatus::Open)
            .count();
        if active_count >= self.policy.limits().max_sessions {
            return Err(PreviewError::new(PreviewErrorCode::SessionLimit));
        }
        let id = PreviewId::new();
        let partition = SiteDataPartition::new(id);
        let window = PreviewWindowSpec::new(id, url.clone(), partition.clone());
        let mut session = SessionState {
            id,
            generation: 1,
            status: PreviewSessionStatus::Open,
            url: url.clone(),
            title: String::new(),
            partition,
            events: VecDeque::new(),
            terminal_events: VecDeque::new(),
            event_bytes: 0,
            next_sequence: 1,
            dropped_events: 0,
            pending_clear: HashMap::new(),
        };
        Self::push_event(
            &mut session,
            PreviewEventKind::Opened { url },
            self.policy.limits(),
        )?;
        let snapshot = Self::snapshot_state(&session);
        state.sessions.insert(id, session);
        Ok(PreviewOpenResult { snapshot, window })
    }

    /// 检查 session/generation/status，所有 callback 在触碰业务数据前必须调用。
    fn ensure_open_generation(
        &self,
        state: &RegistryState,
        id: PreviewId,
        generation: PreviewGeneration,
    ) -> Result<(), PreviewError> {
        let session = state
            .sessions
            .get(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        if session.status == PreviewSessionStatus::Closed {
            return Err(PreviewError::new(PreviewErrorCode::SessionClosed));
        }
        if session.generation != generation {
            return Err(PreviewError::new(PreviewErrorCode::StaleGeneration));
        }
        Ok(())
    }

    /// tombstone 有界化，避免长期打开/关闭网页造成 session map 泄漏。
    fn prune_closed(&self, state: &mut RegistryState) {
        while state.closed_order.len() > self.policy.limits().max_tombstones {
            let Some(id) = state.closed_order.pop_front() else {
                break;
            };
            if state
                .sessions
                .get(&id)
                .is_some_and(|session| session.status == PreviewSessionStatus::Closed)
            {
                state.sessions.remove(&id);
            }
        }
    }

    /// 生成 bounded event，并在空间不足时丢弃最旧的非权威事件。
    fn push_event(
        session: &mut SessionState,
        kind: PreviewEventKind,
        limits: PreviewLimits,
    ) -> Result<PreviewEvent, PreviewError> {
        let sequence = session
            .next_sequence
            .checked_add(1)
            .ok_or(PreviewError::new(PreviewErrorCode::SequenceExhausted))?;
        let event = PreviewEvent {
            session_id: session.id,
            generation: session.generation,
            sequence: session.next_sequence,
            kind,
        };
        let payload_bytes = event.encoded_bytes();
        if payload_bytes > limits.max_event_payload_bytes {
            return Err(PreviewError::new(PreviewErrorCode::EventPayloadTooLarge));
        }
        session.next_sequence = sequence;
        let terminal = is_terminal_kind(&event.kind);
        if terminal && session.terminal_events.len() >= limits.max_terminal_event_count {
            return Err(PreviewError::new(PreviewErrorCode::EventQueueFull));
        }
        // Normal events may be evicted to reserve queue space for terminal outcomes;
        // terminal events themselves are never evicted by a remote page flood.
        while (!terminal && session.events.len() >= limits.max_event_count)
            || session.event_bytes.saturating_add(payload_bytes) > limits.max_event_queue_bytes
        {
            let Some(oldest) = session.events.pop_front() else {
                break;
            };
            session.event_bytes = session.event_bytes.saturating_sub(oldest.encoded_bytes());
            session.dropped_events = session.dropped_events.saturating_add(1);
        }
        if session.event_bytes.saturating_add(payload_bytes) > limits.max_event_queue_bytes {
            return Err(PreviewError::new(PreviewErrorCode::EventQueueFull));
        }
        session.event_bytes = session.event_bytes.saturating_add(payload_bytes);
        if terminal {
            session.terminal_events.push_back(event.clone());
        } else {
            session.events.push_back(event.clone());
        }
        Ok(event)
    }

    /// 生成不含句柄的 snapshot，供 adapter 和 reload UI 安全读取。
    fn snapshot_state(session: &SessionState) -> PreviewSessionSnapshot {
        let window =
            PreviewWindowSpec::new(session.id, session.url.clone(), session.partition.clone());
        PreviewSessionSnapshot {
            id: session.id,
            generation: session.generation,
            status: session.status,
            url: session.url.clone(),
            title: session.title.clone(),
            partition: session.partition.clone(),
            window,
            dropped_events: session.dropped_events,
        }
    }
}

/// UTF-8 安全截断，避免恶意页面标题/错误信息造成无界内存或无效字符串。
fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// checked generation 防止 u64 wrap 让旧 callback 重新匹配。
fn next_generation(current: PreviewGeneration) -> Result<PreviewGeneration, PreviewError> {
    current
        .checked_add(1)
        .ok_or(PreviewError::new(PreviewErrorCode::GenerationExhausted))
}

/// Terminal events必须独立保留，避免页面高频 title/error 把关闭/失败状态挤掉。
fn is_terminal_kind(kind: &PreviewEventKind) -> bool {
    matches!(
        kind,
        PreviewEventKind::SiteDataCleared
            | PreviewEventKind::SiteDataClearFailed
            | PreviewEventKind::Closed
    )
}

/// 按 sequence 合并普通 lane 与 terminal lane，保持 UI 看到的事件顺序。
fn pop_next_event(session: &mut SessionState) -> Option<PreviewEvent> {
    match (session.events.front(), session.terminal_events.front()) {
        (Some(normal), Some(terminal)) if terminal.sequence < normal.sequence => {
            session.terminal_events.pop_front()
        }
        (Some(_), _) => session.events.pop_front(),
        (None, Some(_)) => session.terminal_events.pop_front(),
        (None, None) => None,
    }
}

/// 把 monotonic deadline 投影为有限 UI 信息，不把 Instant 或本机路径暴露出去。
fn deadline_unix_millis(deadline: Instant) -> u64 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    now.saturating_add(remaining)
        .as_millis()
        .min(u64::MAX as u128) as u64
}
