// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Minimal HTTP(S) preview model and Tauri child-window commands.

pub(crate) mod commands;
mod error;
mod model;
mod policy;
mod session;

pub use commands::{
    PREVIEW_EVENT, PreviewCommandHost, PreviewEventsInput, PreviewNavigateInput,
    PreviewSessionInput, PreviewUrlInput, ja_preview_close, ja_preview_events, ja_preview_navigate,
    ja_preview_open, ja_preview_state,
};
pub use error::{PreviewError, PreviewErrorCode};
pub use model::{
    NavigationSource, PreviewEvent, PreviewEventKind, PreviewGeneration, PreviewId, PreviewLimits,
    PreviewNavigationRequest, PreviewOpenResult, PreviewSessionSnapshot, PreviewSessionStatus,
    PreviewUrl, PreviewWindowSpec,
};
pub use policy::{PreviewNavigationDecision, PreviewPolicy};
pub use session::PreviewManager;

#[cfg(test)]
mod tests;
