// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Bounded preview session state and stale callback handling.

use super::error::{PreviewError, PreviewErrorCode};
use super::model::{
    NavigationSource, PreviewEvent, PreviewEventKind, PreviewGeneration, PreviewId, PreviewLimits,
    PreviewNavigationRequest, PreviewOpenResult, PreviewSessionSnapshot, PreviewSessionStatus,
    PreviewUrl, PreviewWindowSpec,
};
use super::policy::{PreviewNavigationDecision, PreviewPolicy};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

/// Owns all in-memory preview sessions for one Tauri application.
#[derive(Clone)]
pub struct PreviewManager {
    policy: Arc<PreviewPolicy>,
    state: Arc<Mutex<RegistryState>>,
}

struct RegistryState {
    sessions: HashMap<PreviewId, SessionState>,
}

struct SessionState {
    id: PreviewId,
    generation: PreviewGeneration,
    status: PreviewSessionStatus,
    url: PreviewUrl,
    title: String,
    events: VecDeque<PreviewEvent>,
    event_bytes: usize,
    next_sequence: u64,
    dropped_events: u64,
}

impl PreviewManager {
    /// Creates the manager after validating its shared URL/event budgets.
    pub fn new(policy: PreviewPolicy) -> Result<Self, PreviewError> {
        policy.limits().validate()?;
        Ok(Self {
            policy: Arc::new(policy),
            state: Arc::new(Mutex::new(RegistryState {
                sessions: HashMap::new(),
            })),
        })
    }

    /// Uses the product's default HTTP(S)-only policy.
    pub fn default_manager() -> Result<Self, PreviewError> {
        Self::new(PreviewPolicy::new()?)
    }

    /// Opens a bounded session and emits its first event.
    pub fn open(&self, raw_url: &str) -> Result<PreviewOpenResult, PreviewError> {
        let url = self.policy.validate_url(raw_url)?;
        let mut state = self.lock_state()?;
        let active = state
            .sessions
            .values()
            .filter(|session| session.status == PreviewSessionStatus::Open)
            .count();
        if active >= self.policy.limits().max_sessions {
            return Err(PreviewError::new(PreviewErrorCode::SessionLimit));
        }
        let id = PreviewId::new();
        let window = PreviewWindowSpec::new(id, url.clone());
        let mut session = SessionState {
            id,
            generation: 1,
            status: PreviewSessionStatus::Open,
            url: url.clone(),
            title: String::new(),
            events: VecDeque::new(),
            event_bytes: 0,
            next_sequence: 1,
            dropped_events: 0,
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

    /// Validates a native navigation before the command calls WebView.navigate.
    pub fn navigation_request(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        source: NavigationSource,
        raw_url: &str,
    ) -> Result<PreviewNavigationRequest, PreviewError> {
        let PreviewNavigationDecision::Allow { url } = self.policy.navigation(source, raw_url)?;
        let state = self.lock_state()?;
        self.ensure_open_generation(&state, id, generation)?;
        Ok(PreviewNavigationRequest {
            session_id: id,
            generation,
            source,
            url,
        })
    }

    /// Commits a user/redirect navigation and advances its callback generation.
    pub fn navigate(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        source: NavigationSource,
        raw_url: &str,
    ) -> Result<PreviewSessionSnapshot, PreviewError> {
        let request = self.navigation_request(id, generation, source, raw_url)?;
        self.commit_navigation(request)
    }

    /// Commits a navigation callback after rechecking its URL and generation.
    pub fn callback_navigation(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        raw_url: &str,
    ) -> Result<PreviewEvent, PreviewError> {
        let request =
            self.navigation_request(id, generation, NavigationSource::Redirect, raw_url)?;
        self.commit_navigation_event(request)
    }

    /// Accepts a bounded title callback from the WebView.
    pub fn callback_title(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        title: &str,
    ) -> Result<PreviewEvent, PreviewError> {
        let mut state = self.lock_state()?;
        let session = self.session_mut(&mut state, id, generation)?;
        let title = truncate_utf8(title, self.policy.limits().max_title_bytes);
        session.title = title.clone();
        Self::push_event(
            session,
            PreviewEventKind::TitleChanged { title },
            self.policy.limits(),
        )
    }

    /// Accepts a bounded load-error callback without returning engine details to IPC.
    pub fn callback_load_error(
        &self,
        id: PreviewId,
        generation: PreviewGeneration,
        message: &str,
    ) -> Result<PreviewEvent, PreviewError> {
        let mut state = self.lock_state()?;
        let session = self.session_mut(&mut state, id, generation)?;
        let message = truncate_utf8(message, self.policy.limits().max_error_bytes);
        Self::push_event(
            session,
            PreviewEventKind::LoadFailed { message },
            self.policy.limits(),
        )
    }

    /// Closes one session idempotently and keeps its final event in memory.
    pub fn close(&self, id: PreviewId) -> Result<PreviewSessionSnapshot, PreviewError> {
        let mut state = self.lock_state()?;
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        if session.status == PreviewSessionStatus::Open {
            session.status = PreviewSessionStatus::Closed;
            Self::push_event(session, PreviewEventKind::Closed, self.policy.limits())?;
        }
        Ok(Self::snapshot_state(session))
    }

    /// Drains a bounded event batch in sequence order.
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
        let mut events = Vec::with_capacity(count.min(session.events.len()));
        for _ in 0..count {
            let Some(event) = session.events.pop_front() else {
                break;
            };
            session.event_bytes = session.event_bytes.saturating_sub(event.encoded_bytes());
            events.push(event);
        }
        Ok(events)
    }

    /// Returns the authoritative state used after a UI reload.
    pub fn snapshot(&self, id: PreviewId) -> Result<PreviewSessionSnapshot, PreviewError> {
        let state = self.lock_state()?;
        state
            .sessions
            .get(&id)
            .map(Self::snapshot_state)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))
    }

    /// Reports active WebViews for app shutdown verification.
    pub fn active_count(&self) -> Result<usize, PreviewError> {
        let state = self.lock_state()?;
        Ok(state
            .sessions
            .values()
            .filter(|session| session.status == PreviewSessionStatus::Open)
            .count())
    }

    /// Closes every open model session during native shutdown.
    pub fn shutdown(&self) -> Result<(), PreviewError> {
        let ids = {
            let state = self.lock_state()?;
            state
                .sessions
                .values()
                .filter(|session| session.status == PreviewSessionStatus::Open)
                .map(|session| session.id)
                .collect::<Vec<_>>()
        };
        for id in ids {
            self.close(id)?;
        }
        Ok(())
    }

    /// Commits a validated navigation and returns the new snapshot.
    fn commit_navigation(
        &self,
        request: PreviewNavigationRequest,
    ) -> Result<PreviewSessionSnapshot, PreviewError> {
        let id = request.session_id;
        self.commit_navigation_event(request)?;
        self.snapshot(id)
    }

    /// Updates URL/title/generation and appends one ordered event.
    fn commit_navigation_event(
        &self,
        request: PreviewNavigationRequest,
    ) -> Result<PreviewEvent, PreviewError> {
        let mut state = self.lock_state()?;
        let session = self.session_mut(&mut state, request.session_id, request.generation)?;
        session.generation = next_generation(session.generation)?;
        session.url = request.url.clone();
        session.title.clear();
        Self::push_event(
            session,
            PreviewEventKind::NavigationCommitted {
                source: request.source,
                url: request.url,
            },
            self.policy.limits(),
        )
    }

    /// Looks up one open generation while holding the registry lock.
    fn session_mut<'a>(
        &self,
        state: &'a mut RegistryState,
        id: PreviewId,
        generation: PreviewGeneration,
    ) -> Result<&'a mut SessionState, PreviewError> {
        let session = state
            .sessions
            .get_mut(&id)
            .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
        if session.status == PreviewSessionStatus::Closed {
            return Err(PreviewError::new(PreviewErrorCode::SessionClosed));
        }
        if session.generation != generation {
            return Err(PreviewError::new(PreviewErrorCode::StaleGeneration));
        }
        Ok(session)
    }

    /// Keeps the lock poison failure explicit instead of using a stale map.
    fn lock_state(&self) -> Result<MutexGuard<'_, RegistryState>, PreviewError> {
        self.state
            .lock()
            .map_err(|_| PreviewError::new(PreviewErrorCode::InternalStateUnavailable))
    }

    /// Performs a read-only generation/status check for a navigation request.
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

    /// Adds one event while dropping only old non-terminal history at the byte cap.
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
        while session.events.len() >= limits.max_event_count
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
        session.events.push_back(event.clone());
        Ok(event)
    }

    /// Projects model state without exposing a WebView handle.
    fn snapshot_state(session: &SessionState) -> PreviewSessionSnapshot {
        PreviewSessionSnapshot {
            id: session.id,
            generation: session.generation,
            status: session.status,
            url: session.url.clone(),
            title: session.title.clone(),
            window: PreviewWindowSpec::new(session.id, session.url.clone()),
            dropped_events: session.dropped_events,
        }
    }
}

/// UTF-8 safe bounded title/error projection.
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

/// Checked generation avoids wraparound revalidating old callbacks.
fn next_generation(current: PreviewGeneration) -> Result<PreviewGeneration, PreviewError> {
    current
        .checked_add(1)
        .ok_or(PreviewError::new(PreviewErrorCode::GenerationExhausted))
}
