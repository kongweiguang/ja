// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Small versioned settings facade with one backup file.

use super::atomic::{atomic_copy, atomic_replace, read_bounded};
use super::model::{SETTINGS_SCHEMA_VERSION, SettingsDocument, SettingsModelError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

const SETTINGS_FILE: &str = "settings.json";
const SETTINGS_BACKUP_FILE: &str = "settings.json.bak";
const MAX_SETTINGS_BYTES: usize = 256 * 1024;
const MAX_JSON_DEPTH: usize = 16;
const MAX_SECRET_KEY_LENGTH: usize = 96;

/// Indicates which bounded source supplied the current settings document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LoadSource {
    Default,
    Primary,
    Backup,
}

/// Safe load diagnostics contain no filesystem path or secret value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadedSettings {
    pub document: SettingsDocument,
    pub source: LoadSource,
    pub recovered: bool,
    pub migrated: bool,
}

/// Stable errors prevent parser/native detail from crossing IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SettingsError {
    InvalidRoot,
    Io,
    TooLarge,
    InvalidJson,
    TooDeep,
    UnknownField,
    SecretField,
    UnsupportedSchema,
    InvalidDocument,
    CorruptRecovery,
    RevisionConflict,
    Unavailable,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidRoot => "invalid settings root",
            Self::Io => "settings I/O failed",
            Self::TooLarge => "settings document too large",
            Self::InvalidJson => "settings JSON invalid",
            Self::TooDeep => "settings JSON too deep",
            Self::UnknownField => "settings field unknown",
            Self::SecretField => "secret field is not allowed in settings",
            Self::UnsupportedSchema => "settings schema unsupported",
            Self::InvalidDocument => "settings document invalid",
            Self::CorruptRecovery => "settings recovery failed",
            Self::RevisionConflict => "settings revision conflict",
            Self::Unavailable => "settings store unavailable",
        })
    }
}

impl std::error::Error for SettingsError {}

/// Owns the settings directory and serializes all read/write transactions.
#[derive(Clone)]
pub struct SettingsStore {
    root: Arc<PathBuf>,
    lock: Arc<Mutex<()>>,
}

impl SettingsStore {
    /// Canonicalizes the app-private directory and removes only stale temp
    /// names produced by the settings writer.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SettingsError> {
        let raw_root = root.into();
        fs::create_dir_all(&raw_root).map_err(|_| SettingsError::Io)?;
        let root = fs::canonicalize(raw_root).map_err(|_| SettingsError::InvalidRoot)?;
        cleanup_temp_files(&root)?;
        Ok(Self {
            root: Arc::new(root),
            lock: Arc::new(Mutex::new(())),
        })
    }

    /// Loads primary, then backup, and uses defaults only when neither exists.
    pub fn load(&self) -> Result<LoadedSettings, SettingsError> {
        let _guard = self.lock_store()?;
        let primary = self.load_file(&self.primary_path());
        if let Ok((document, migrated, old_bytes)) = primary {
            if migrated {
                // Preserve the exact old document before replacing it with a
                // migrated schema so a failed future migration is recoverable.
                self.backup_primary_locked(&old_bytes)?;
                self.persist_document_locked(&document, false)?;
            }
            return Ok(LoadedSettings {
                document,
                source: LoadSource::Primary,
                recovered: false,
                migrated,
            });
        }

        if let Ok((document, migrated, _)) = self.load_file(&self.backup_path()) {
            self.persist_document_locked(&document, false)?;
            return Ok(LoadedSettings {
                document,
                source: LoadSource::Backup,
                recovered: true,
                migrated,
            });
        }

        if !self.primary_path().exists() && !self.backup_path().exists() {
            return Ok(LoadedSettings {
                document: SettingsDocument::default(),
                source: LoadSource::Default,
                recovered: false,
                migrated: false,
            });
        }
        Err(SettingsError::CorruptRecovery)
    }

    /// Saves only a new revision, or an initial revision when no file exists;
    /// unrestricted last-writer-wins saves are intentionally unavailable.
    pub fn save(&self, document: &SettingsDocument) -> Result<(), SettingsError> {
        let _guard = self.lock_store()?;
        document.validate().map_err(map_model_error)?;
        let primary = self.primary_path();
        let backup = self.backup_path();
        if primary.exists() || backup.exists() {
            let current = self
                .load_current_locked()?
                .ok_or(SettingsError::CorruptRecovery)?;
            let expected = current
                .revision
                .checked_add(1)
                .ok_or(SettingsError::RevisionConflict)?;
            if document.revision != expected {
                return Err(SettingsError::RevisionConflict);
            }
            self.persist_document_locked(document, primary.exists())
        } else {
            if document.revision > 1 {
                return Err(SettingsError::RevisionConflict);
            }
            self.persist_document_locked(document, false)
        }
    }

    /// Performs an explicit checked CAS for UI edits from a known snapshot.
    pub fn compare_and_save(
        &self,
        expected_revision: u64,
        document: &SettingsDocument,
    ) -> Result<SettingsDocument, SettingsError> {
        let _guard = self.lock_store()?;
        let current = self.load_current_locked()?;
        let current_revision = current.as_ref().map_or(0, |value| value.revision);
        let next_revision = expected_revision
            .checked_add(1)
            .ok_or(SettingsError::RevisionConflict)?;
        if current_revision != expected_revision || document.revision != next_revision {
            return Err(SettingsError::RevisionConflict);
        }
        document.validate().map_err(map_model_error)?;
        self.persist_document_locked(document, current.is_some())?;
        Ok(document.clone())
    }

    /// Refuses to continue after a poisoned transaction mutex.
    fn lock_store(&self) -> Result<MutexGuard<'_, ()>, SettingsError> {
        self.lock.lock().map_err(|_| SettingsError::Unavailable)
    }

    /// Uses primary for CAS, but permits backup recovery when primary is gone.
    fn load_current_locked(&self) -> Result<Option<SettingsDocument>, SettingsError> {
        match self.load_file(&self.primary_path()) {
            Ok((document, _, _)) => Ok(Some(document)),
            Err(_) if self.primary_path().exists() => Err(SettingsError::CorruptRecovery),
            Err(_) => match self.load_file(&self.backup_path()) {
                Ok((document, _, _)) => Ok(Some(document)),
                Err(_) if !self.backup_path().exists() => Ok(None),
                Err(_) => Err(SettingsError::CorruptRecovery),
            },
        }
    }

    /// Reads bounded bytes and returns the parsed document plus migration input.
    fn load_file(&self, path: &Path) -> Result<(SettingsDocument, bool, Vec<u8>), SettingsError> {
        let bytes = read_bounded(path, MAX_SETTINGS_BYTES).map_err(|error| {
            if error.kind() == std::io::ErrorKind::InvalidData {
                SettingsError::TooLarge
            } else {
                SettingsError::Io
            }
        })?;
        let (document, migrated) = parse_document(&bytes)?;
        Ok((document, migrated, bytes))
    }

    /// Copies the old primary to the single backup before migration replaces it.
    fn backup_primary_locked(&self, old_bytes: &[u8]) -> Result<(), SettingsError> {
        atomic_replace(&self.backup_path(), old_bytes).map_err(|_| SettingsError::Io)
    }

    /// Serializes and atomically publishes a document, optionally backing up
    /// the currently valid primary first.
    fn persist_document_locked(
        &self,
        document: &SettingsDocument,
        backup_current: bool,
    ) -> Result<(), SettingsError> {
        let bytes =
            serde_json::to_vec_pretty(document).map_err(|_| SettingsError::InvalidDocument)?;
        if bytes.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsError::TooLarge);
        }
        if backup_current && self.primary_path().exists() {
            atomic_copy(
                &self.primary_path(),
                &self.backup_path(),
                MAX_SETTINGS_BYTES,
            )
            .map_err(|_| SettingsError::Io)?;
        }
        atomic_replace(&self.primary_path(), &bytes).map_err(|_| SettingsError::Io)
    }

    fn primary_path(&self) -> PathBuf {
        self.root.join(SETTINGS_FILE)
    }

    fn backup_path(&self) -> PathBuf {
        self.root.join(SETTINGS_BACKUP_FILE)
    }
}

/// Removes only interrupted temp files, never user settings or the backup.
fn cleanup_temp_files(root: &Path) -> Result<(), SettingsError> {
    for entry in fs::read_dir(root).map_err(|_| SettingsError::Io)? {
        let entry = entry.map_err(|_| SettingsError::Io)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if (name.starts_with(".settings.json.tmp-") || name.starts_with(".settings.json.old-"))
            && entry.file_type().map_err(|_| SettingsError::Io)?.is_file()
        {
            fs::remove_file(entry.path()).map_err(|_| SettingsError::Io)?;
        }
    }
    Ok(())
}

/// Parses strict bounded JSON before typed decoding and migration.
fn parse_document(bytes: &[u8]) -> Result<(SettingsDocument, bool), SettingsError> {
    if bytes.len() > MAX_SETTINGS_BYTES {
        return Err(SettingsError::TooLarge);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| SettingsError::InvalidJson)?;
    if json_depth(&value, 0) > MAX_JSON_DEPTH {
        return Err(SettingsError::TooDeep);
    }
    reject_sensitive_keys(&value)?;
    let schema_version = value
        .get("schemaVersion")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    match u32::try_from(schema_version).unwrap_or(u32::MAX) {
        SETTINGS_SCHEMA_VERSION => {
            let document = serde_json::from_value(value).map_err(map_serde_error)?;
            Ok((document, false))
        }
        0 => migrate_v0(value),
        _ => Err(SettingsError::UnsupportedSchema),
    }
}

/// Converts the prototype schema without discarding the original bytes.
fn migrate_v0(mut value: Value) -> Result<(SettingsDocument, bool), SettingsError> {
    if let Value::Object(ref mut object) = value {
        object.remove("schemaVersion");
    } else {
        return Err(SettingsError::InvalidDocument);
    }
    let legacy: LegacySettingsV0 = serde_json::from_value(value).map_err(map_serde_error)?;
    let document = SettingsDocument {
        schema_version: SETTINGS_SCHEMA_VERSION,
        revision: legacy.revision.unwrap_or(0),
        theme: legacy.theme.unwrap_or_default(),
        active_profile_revision: legacy.active_profile_revision,
        profiles: legacy.profiles,
        mcp_servers: legacy.mcp_servers,
        window: legacy.window.unwrap_or_default(),
    };
    document.validate().map_err(map_model_error)?;
    Ok((document, true))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacySettingsV0 {
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    theme: Option<super::model::ThemePreference>,
    #[serde(default)]
    active_profile_revision: Option<String>,
    #[serde(default)]
    profiles: Vec<super::model::ProfileSetting>,
    #[serde(default)]
    mcp_servers: Vec<super::model::McpServerSetting>,
    #[serde(default)]
    window: Option<super::model::WindowSettings>,
}

/// Measures JSON nesting iteratively so strict depth is independent of DTOs.
fn json_depth(value: &Value, current: usize) -> usize {
    let mut max_depth = current;
    let mut pending = vec![(value, current)];
    while let Some((node, depth)) = pending.pop() {
        max_depth = max_depth.max(depth);
        match node {
            Value::Array(values) => pending.extend(values.iter().map(|child| (child, depth + 1))),
            Value::Object(values) => {
                pending.extend(values.values().map(|child| (child, depth + 1)))
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    max_depth
}

/// Rejects secret-shaped keys before any future schema extension can persist them.
fn reject_sensitive_keys(value: &Value) -> Result<(), SettingsError> {
    let mut pending = vec![value];
    while let Some(node) = pending.pop() {
        match node {
            Value::Object(fields) => {
                for (key, child) in fields {
                    if key.len() > MAX_SECRET_KEY_LENGTH || is_sensitive_key(key) {
                        return Err(SettingsError::SecretField);
                    }
                    pending.push(child);
                }
            }
            Value::Array(values) => pending.extend(values.iter()),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }
    Ok(())
}

/// Normalizes separators and casing for the persistence secret deny-list.
fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    matches!(
        normalized.as_str(),
        "secret"
            | "secretvalue"
            | "password"
            | "apikey"
            | "token"
            | "accesstoken"
            | "refreshtoken"
            | "authorization"
            | "bearer"
            | "privatekey"
    ) || normalized.contains("secret")
        || normalized.contains("password")
        || normalized.contains("token")
        || normalized.contains("apikey")
        || normalized.contains("authorization")
}

fn map_model_error(error: SettingsModelError) -> SettingsError {
    match error {
        SettingsModelError::UnsupportedSchema => SettingsError::UnsupportedSchema,
        _ => SettingsError::InvalidDocument,
    }
}

/// Uses parser diagnostics only for category mapping; detail never leaves Rust.
fn map_serde_error(error: serde_json::Error) -> SettingsError {
    if error.to_string().contains("unknown field") {
        SettingsError::UnknownField
    } else {
        SettingsError::InvalidDocument
    }
}
