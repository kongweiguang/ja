// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Small preview DTOs shared by the policy, session registry and Tauri host.

use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use uuid::Uuid;

/// Opaque preview session identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PreviewId(Uuid);

impl PreviewId {
    /// Random ids prevent a page from guessing another preview owner.
    pub(crate) fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Stable Tauri label contains no URL or filesystem data.
    pub(crate) fn label(self) -> String {
        format!("preview_{}", self.0.simple())
    }
}

impl Display for PreviewId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Generation rejects callbacks from a page that has already navigated.
pub type PreviewGeneration = u64;

/// URL constructed only after `PreviewPolicy` validates HTTP(S) input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PreviewUrl(String);

impl PreviewUrl {
    /// Keeps construction private to the policy boundary.
    pub(crate) fn from_normalized(value: String) -> Self {
        Self(value)
    }

    /// Returns the normalized URL for Tauri's external WebView URL.
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
    /// Deserialization uses the same policy so JSON cannot bypass scheme checks.
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

/// Preview lifecycle visible to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewSessionStatus {
    Open,
    Closed,
}

/// Source is kept small because popup/browser automation is out of scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NavigationSource {
    User,
    Redirect,
}

/// Tauri child-window description; the window label has no main capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewWindowSpec {
    label: String,
    url: PreviewUrl,
}

impl PreviewWindowSpec {
    /// Creates a label that is absent from the main-window capability scope.
    pub(crate) fn new(id: PreviewId, url: PreviewUrl) -> Self {
        Self {
            label: id.label(),
            url,
        }
    }

    /// Returns the Tauri child window label.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the URL already checked by policy.
    pub fn url(&self) -> &PreviewUrl {
        &self.url
    }
}

/// One bounded event emitted to the main UI.
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
    TitleChanged {
        title: String,
    },
    LoadFailed {
        message: String,
    },
    Closed,
}

/// Event identity allows reload projection and stale callback rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewEvent {
    pub session_id: PreviewId,
    pub generation: PreviewGeneration,
    pub sequence: u64,
    pub kind: PreviewEventKind,
}

/// Open result contains only UI-safe model state and a child-window spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewOpenResult {
    pub snapshot: PreviewSessionSnapshot,
    pub window: PreviewWindowSpec,
}

/// Authoritative snapshot used after reload or late event subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PreviewSessionSnapshot {
    pub id: PreviewId,
    pub generation: PreviewGeneration,
    pub status: PreviewSessionStatus,
    pub url: PreviewUrl,
    pub title: String,
    pub window: PreviewWindowSpec,
    pub dropped_events: u64,
}

/// Navigation validation result used by the command before WebView.navigate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewNavigationRequest {
    pub(crate) session_id: PreviewId,
    pub(crate) generation: PreviewGeneration,
    pub(crate) source: NavigationSource,
    pub(crate) url: PreviewUrl,
}

/// Bounded registry budgets prevent a remote page from filling the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewLimits {
    pub max_sessions: usize,
    pub max_url_bytes: usize,
    pub max_title_bytes: usize,
    pub max_error_bytes: usize,
    pub max_event_count: usize,
    pub max_event_payload_bytes: usize,
    pub max_event_queue_bytes: usize,
}

impl Default for PreviewLimits {
    /// These limits cover ordinary docs pages without creating unbounded state.
    fn default() -> Self {
        Self {
            max_sessions: 8,
            max_url_bytes: 8 * 1024,
            max_title_bytes: 1024,
            max_error_bytes: 4 * 1024,
            max_event_count: 512,
            max_event_payload_bytes: 64 * 1024,
            max_event_queue_bytes: 2 * 1024 * 1024,
        }
    }
}

impl PreviewLimits {
    /// Checks relationships before a registry starts accepting pages.
    pub(crate) fn validate(self) -> Result<Self, super::error::PreviewError> {
        let valid = self.max_sessions > 0
            && self.max_sessions <= 64
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
            && self.max_event_queue_bytes <= 64 * 1024 * 1024;
        valid.then_some(self).ok_or(super::error::PreviewError::new(
            super::error::PreviewErrorCode::InvalidConfig,
        ))
    }
}

impl PreviewEvent {
    /// Counts serialized bytes so event queue accounting matches IPC reality.
    pub(crate) fn encoded_bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }
}
