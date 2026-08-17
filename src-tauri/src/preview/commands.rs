// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Tauri/WebView adapter for the URL-only preview model.
//!
//! Preview windows use labels that are absent from the main capability file,
//! so they receive no Tauri commands.  The model remains authoritative for
//! URL/generation checks; Wry callbacks only report already-bounded events.

use super::error::{PreviewError, PreviewErrorCode};
use super::model::{NavigationSource, PreviewEvent, PreviewId};
use super::session::PreviewManager;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use url::Url;

/// Event name shared by preview windows and the main UI.
pub const PREVIEW_EVENT: &str = "ja://preview";

/// Managed preview state; native window handles remain owned by Tauri.
#[derive(Clone)]
pub struct PreviewCommandHost {
    manager: PreviewManager,
}

impl Default for PreviewCommandHost {
    /// Constructs the same validated default policy used by pure model tests.
    fn default() -> Self {
        Self {
            manager: PreviewManager::default_manager()
                .unwrap_or_else(|_| panic!("default preview policy is invalid")),
        }
    }
}

impl PreviewCommandHost {
    /// Returns the shared manager for shutdown and command adapters.
    pub fn manager(&self) -> &PreviewManager {
        &self.manager
    }

    /// Closes model sessions; Tauri closes the corresponding windows with the
    /// normal app lifecycle after this method returns.
    pub fn shutdown(&self) -> Result<(), PreviewError> {
        self.manager.shutdown()
    }
}

/// Opens one validated HTTP(S) URL in an isolated child WebView.
#[tauri::command]
pub async fn ja_preview_open(
    input: PreviewUrlInput,
    app: tauri::AppHandle,
    state: tauri::State<'_, PreviewCommandHost>,
) -> Result<super::model::PreviewOpenResult, PreviewError> {
    let opened = state.manager.open(&input.url)?;
    let parsed = Url::parse(opened.window.url().as_str())
        .map_err(|_| PreviewError::new(PreviewErrorCode::UrlInvalid))?;
    let id = opened.snapshot.id;
    let generation = Arc::new(AtomicU64::new(opened.snapshot.generation));
    let callback_manager = state.manager.clone();
    let callback_app = app.clone();
    let callback_generation = generation.clone();
    let label = opened.window.label().to_owned();
    let title_manager = state.manager.clone();
    let title_app = app.clone();
    let title_generation = generation.clone();
    let window_result = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(parsed))
        .title("JA Preview")
        // A preview is an external page, so navigation is admitted only through
        // the same URL policy that validated its initial address.
        .on_navigation(move |url| {
            let current = callback_generation.load(Ordering::Acquire);
            match callback_manager.callback_navigation(id, current, url.as_str()) {
                Ok(event) => {
                    callback_generation.store(event.generation, Ordering::Release);
                    emit_preview_event(&callback_app, event);
                    true
                }
                Err(_) => false,
            }
        })
        // Popups must not inherit an opener or silently create a privileged
        // window; users can open another URL through the explicit preview command.
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
        // Preview never writes downloads to the user's filesystem.
        .on_download(|_, _| false)
        .on_document_title_changed(move |_window, title| {
            let current = title_generation.load(Ordering::Acquire);
            if let Ok(event) = title_manager.callback_title(id, current, &title) {
                emit_preview_event(&title_app, event);
            }
        })
        .build();
    // A native build failure must not leave a model session that has no window
    // owner; closing it here keeps the active-session limit truthful.
    let window = match window_result {
        Ok(window) => window,
        Err(_) => {
            let _ = state.manager.close(id);
            return Err(PreviewError::new(PreviewErrorCode::DependencyRequest));
        }
    };

    let close_manager = state.manager.clone();
    window.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            let _ = close_manager.close(id);
        }
    });
    Ok(opened)
}

/// Navigates an existing preview after policy validation; the WebView callback
/// commits the authoritative generation and emits the resulting event.
#[tauri::command]
pub fn ja_preview_navigate(
    input: PreviewNavigateInput,
    app: tauri::AppHandle,
    state: tauri::State<'_, PreviewCommandHost>,
) -> Result<super::model::PreviewSessionSnapshot, PreviewError> {
    let request = state.manager.navigation_request(
        input.session_id,
        input.generation,
        input.source,
        &input.url,
    )?;
    let label = state
        .manager
        .snapshot(input.session_id)?
        .window
        .label()
        .to_owned();
    let window = app
        .get_webview_window(&label)
        .ok_or(PreviewError::new(PreviewErrorCode::SessionNotFound))?;
    let url = Url::parse(request.url.as_str())
        .map_err(|_| PreviewError::new(PreviewErrorCode::UrlInvalid))?;
    window
        .navigate(url)
        .map_err(|_| PreviewError::new(PreviewErrorCode::DependencyRequest))?;
    state.manager.snapshot(input.session_id)
}

/// Closes the model session and then asks Tauri to close its matching window.
#[tauri::command]
pub fn ja_preview_close(
    input: PreviewSessionInput,
    app: tauri::AppHandle,
    state: tauri::State<'_, PreviewCommandHost>,
) -> Result<super::model::PreviewSessionSnapshot, PreviewError> {
    let snapshot = state.manager.close(input.session_id)?;
    if let Some(window) = app.get_webview_window(snapshot.window.label()) {
        window
            .close()
            .map_err(|_| PreviewError::new(PreviewErrorCode::DependencyRequest))?;
    }
    Ok(snapshot)
}

/// Returns the bounded event batch for reload/reconnect projection.
#[tauri::command]
pub fn ja_preview_events(
    input: PreviewEventsInput,
    state: tauri::State<'_, PreviewCommandHost>,
) -> Result<Vec<PreviewEvent>, PreviewError> {
    state
        .manager
        .drain_events(input.session_id, input.max_events)
}

/// Returns the current authoritative preview snapshot.
#[tauri::command]
pub fn ja_preview_state(
    input: PreviewSessionInput,
    state: tauri::State<'_, PreviewCommandHost>,
) -> Result<super::model::PreviewSessionSnapshot, PreviewError> {
    state.manager.snapshot(input.session_id)
}

/// Raw URL input is revalidated by the model before any WebView is built.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewUrlInput {
    pub url: String,
}

/// Identifies a preview session without exposing a native window handle.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewSessionInput {
    pub session_id: PreviewId,
}

/// Navigates a session through one URL/generation policy path.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewNavigateInput {
    pub session_id: PreviewId,
    pub generation: u64,
    pub source: NavigationSource,
    pub url: String,
}

/// Limits one drain call even if a stale UI asks for a huge batch.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreviewEventsInput {
    pub session_id: PreviewId,
    #[serde(default = "default_event_limit")]
    pub max_events: usize,
}

fn default_event_limit() -> usize {
    128
}

/// Emits only typed, bounded model events; failures do not expose WebView
/// diagnostics to the page that caused them.
fn emit_preview_event(app: &tauri::AppHandle, event: PreviewEvent) {
    if let Err(error) = app.emit(PREVIEW_EVENT, event) {
        tracing::debug!(error = %error, "preview event delivery failed");
    }
}
