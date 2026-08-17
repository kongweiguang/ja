// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Minimal Tauri settings/credential commands.
//!
//! JSON settings and native credentials have separate adapters so a model key
//! can never be serialized into the ordinary settings document.

use super::SettingsDocument;
use super::credentials::{CredentialPurpose, CredentialVault, NativeKeyringBackend, SecretError};
use super::model::CredentialRef;
use super::store::{LoadedSettings, SettingsError, SettingsStore};
use serde::Deserialize;
use std::path::PathBuf;

/// Managed app-private settings state; the root is selected by Tauri setup.
#[derive(Clone)]
pub struct SettingsCommandHost {
    store: SettingsStore,
}

impl SettingsCommandHost {
    /// Creates the host only after the app-data directory has been chosen by
    /// the composition root, avoiding a cwd-relative settings file.
    pub fn new(root: PathBuf) -> Result<Self, SettingsError> {
        Ok(Self {
            store: SettingsStore::new(root)?,
        })
    }

    /// Loads primary/backup/default state through the atomic store.
    pub fn load(&self) -> Result<LoadedSettings, SettingsError> {
        self.store.load()
    }

    /// Saves a checked next revision so two windows cannot silently overwrite.
    pub fn compare_and_save(
        &self,
        expected_revision: u64,
        document: &SettingsDocument,
    ) -> Result<SettingsDocument, SettingsError> {
        self.store.compare_and_save(expected_revision, document)
    }
}

/// Reads the current non-secret settings document.
#[tauri::command]
pub fn ja_settings_load(
    state: tauri::State<'_, SettingsCommandHost>,
) -> Result<LoadedSettings, SettingsError> {
    state.load()
}

/// Writes a validated document using an explicit revision check.
#[tauri::command]
pub fn ja_settings_save(
    input: SettingsSaveInput,
    state: tauri::State<'_, SettingsCommandHost>,
) -> Result<SettingsDocument, SettingsError> {
    state.compare_and_save(input.expected_revision, &input.document)
}

/// Stores a provider/MCP secret in the native keyring and returns no secret.
#[tauri::command]
pub fn ja_settings_set_credential(input: SettingsCredentialSetInput) -> Result<(), SecretError> {
    let reference =
        CredentialRef::parse(&input.reference).map_err(|_| SecretError::InvalidReference)?;
    CredentialVault::new(NativeKeyringBackend).set(input.purpose, &reference, &input.secret)
}

/// Deletes a native keyring entry by opaque reference.
#[tauri::command]
pub fn ja_settings_delete_credential(
    input: SettingsCredentialDeleteInput,
) -> Result<(), SecretError> {
    let reference =
        CredentialRef::parse(&input.reference).map_err(|_| SecretError::InvalidReference)?;
    CredentialVault::new(NativeKeyringBackend).delete(input.purpose, &reference)
}

/// Settings writes carry the full document so the backend can enforce schema
/// and expected-revision checks in one atomic operation.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsSaveInput {
    pub expected_revision: u64,
    pub document: SettingsDocument,
}

/// Input for creating/replacing a native credential.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsCredentialSetInput {
    pub purpose: CredentialPurpose,
    pub reference: String,
    pub secret: String,
}

/// Input for removing a native credential without exposing its value.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettingsCredentialDeleteInput {
    pub purpose: CredentialPurpose,
    pub reference: String,
}
