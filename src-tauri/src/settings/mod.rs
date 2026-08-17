// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Durable preferences and credential references.
//!
//! This module deliberately keeps the non-sensitive JSON document, the native
//! credential store, and the runtime-only delivery object as separate types.
//! That separation prevents a future Tauri command from accidentally
//! serializing a provider secret while still allowing the Rust sidecar bridge
//! to deliver it through its private stdin channel.

mod atomic;
pub(crate) mod commands;
mod credentials;
mod model;
mod store;

#[cfg(test)]
mod tests;

pub use commands::{
    SettingsCommandHost, SettingsCredentialDeleteInput, SettingsCredentialSetInput,
    SettingsSaveInput, ja_settings_delete_credential, ja_settings_load, ja_settings_save,
    ja_settings_set_credential,
};
pub use credentials::{
    CredentialPurpose, CredentialVault, NativeKeyringBackend, RuntimeSecretDelivery, SecretBackend,
    SecretDeliveryChannel, SecretDeliveryError, SecretError,
};
pub use model::{
    ApiProtocol, CredentialRef, McpServerSetting, ProfileSetting, SettingsDocument,
    SettingsModelError, ThemePreference, WindowSettings,
};
pub use store::{LoadSource, LoadedSettings, SettingsError, SettingsStore};
