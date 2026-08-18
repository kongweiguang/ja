// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

use super::credentials::{
    CredentialPurpose, CredentialVault, NativeKeyringBackend, RuntimeSecretDelivery, SecretBackend,
    SecretDeliveryChannel, SecretDeliveryError, SecretError,
};
use super::model::{
    AccessMode, ApiProtocol, CredentialRef, McpAuthKind, McpAuthSetting, McpServerSetting,
    ProfileSetting, SettingsDocument,
};
use super::store::{LoadSource, SettingsError, SettingsStore};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

fn test_root(label: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!("ja-settings-{label}-{suffix}"))
}

fn remove_root(path: &PathBuf) {
    let _ = fs::remove_dir_all(path);
}

#[test]
fn missing_settings_use_safe_defaults_without_creating_a_file() {
    let root = test_root("defaults");
    let store = SettingsStore::new(&root).expect("store");
    let loaded = store.load().expect("load");
    assert_eq!(loaded.source, LoadSource::Default);
    assert_eq!(loaded.document, SettingsDocument::default());
    assert!(!root.join("settings.json").exists());
    remove_root(&root);
}

#[test]
fn save_and_compare_and_save_preserve_revision_and_backup() {
    let root = test_root("revision");
    let store = SettingsStore::new(&root).expect("store");
    let first = SettingsDocument {
        revision: 1,
        ..SettingsDocument::default()
    };
    store.save(&first).expect("first save");

    let mut second = first.clone();
    second.revision = 2;
    store
        .compare_and_save(1, &second)
        .expect("compare and save");
    assert!(root.join("settings.json.bak").is_file());
    assert!(matches!(
        store.compare_and_save(1, &second),
        Err(SettingsError::RevisionConflict)
    ));
    remove_root(&root);
}

#[test]
fn legacy_document_migrates_and_writes_schema_version() {
    let root = test_root("migration");
    let store = SettingsStore::new(&root).expect("store");
    fs::write(
        root.join("settings.json"),
        br#"{"theme":"dark","profiles":[],"mcpServers":[]}"#,
    )
    .expect("legacy document");
    let loaded = store.load().expect("migration");
    assert!(loaded.migrated);
    assert_eq!(loaded.document.schema_version, 1);
    let bytes = fs::read(root.join("settings.json")).expect("migrated bytes");
    assert!(String::from_utf8_lossy(&bytes).contains("schemaVersion"));
    assert_eq!(
        fs::read(root.join("settings.json.bak")).expect("legacy backup"),
        br#"{"theme":"dark","profiles":[],"mcpServers":[]}"#
    );
    remove_root(&root);
}

#[test]
fn unknown_and_secret_fields_fail_closed() {
    let root = test_root("strict");
    let store = SettingsStore::new(&root).expect("store");
    fs::write(
        root.join("settings.json"),
        br#"{"schemaVersion":1,"unexpected":true}"#,
    )
    .expect("unknown document");
    assert!(matches!(store.load(), Err(SettingsError::CorruptRecovery)));

    fs::write(
        root.join("settings.json"),
        br#"{"schemaVersion":1,"apiKey":"never persist"}"#,
    )
    .expect("secret document");
    assert!(matches!(store.load(), Err(SettingsError::CorruptRecovery)));
    remove_root(&root);
}

#[test]
fn backup_recovers_a_corrupted_primary_without_exposing_file_path() {
    let root = test_root("recovery");
    let store = SettingsStore::new(&root).expect("store");
    let first = SettingsDocument {
        revision: 1,
        ..SettingsDocument::default()
    };
    store.save(&first).expect("first save");
    let mut second = first.clone();
    second.revision = 2;
    store.save(&second).expect("second save");
    fs::write(root.join("settings.json"), b"{not-json").expect("corrupt primary");

    let loaded = store.load().expect("recover");
    assert_eq!(loaded.source, LoadSource::Backup);
    assert!(loaded.recovered);
    assert_eq!(loaded.document.revision, 1);
    assert!(!format!("{loaded:?}").contains("settings.json"));
    remove_root(&root);
}

#[test]
fn missing_primary_uses_backup_for_checked_save() {
    let root = test_root("backup-cas");
    let store = SettingsStore::new(&root).expect("store");
    let first = SettingsDocument {
        revision: 1,
        ..SettingsDocument::default()
    };
    store.save(&first).expect("first save");
    let mut second = first.clone();
    second.revision = 2;
    store.save(&second).expect("second save");
    fs::remove_file(root.join("settings.json")).expect("remove primary");

    let mut third = second.clone();
    // The backup is the previous durable revision, so the next checked save
    // must continue from that recovered revision rather than skipping one.
    third.revision = 2;
    store.save(&third).expect("save from backup");
    assert_eq!(store.load().expect("load").document.revision, 2);
    remove_root(&root);
}

#[test]
fn provider_base_url_rejects_query_and_fragment() {
    let mut document = SettingsDocument::default();
    document.profiles.push(ProfileSetting {
        profile_revision: "profile_1".to_owned(),
        name: "OpenAI".to_owned(),
        provider: "openai".to_owned(),
        protocol: ApiProtocol::OpenAiResponses,
        model: "model".to_owned(),
        base_url: Some("https://example.com/v1?token=hidden".to_owned()),
        credential_ref: None,
        supports_vision: false,
        access_mode: Default::default(),
        skill_revisions: Vec::new(),
        mcp_revisions: None,
    });
    assert!(document.validate().is_err());

    document.profiles[0].base_url = Some("https://example.com/v1#fragment".to_owned());
    assert!(document.validate().is_err());
}

#[test]
fn responses_protocol_is_recognized_but_not_activatable() {
    let mut document = SettingsDocument::default();
    document.profiles.push(ProfileSetting {
        profile_revision: "profile_responses".to_owned(),
        name: "Reserved".to_owned(),
        provider: "openai".to_owned(),
        protocol: ApiProtocol::OpenAiResponses,
        model: "model".to_owned(),
        base_url: Some("https://example.com/v1".to_owned()),
        credential_ref: None,
        supports_vision: false,
        access_mode: Default::default(),
        skill_revisions: Vec::new(),
        mcp_revisions: None,
    });
    assert!(matches!(
        document.validate(),
        Err(super::model::SettingsModelError::UnsupportedProtocol)
    ));
}

#[test]
fn structured_profile_and_mcp_references_round_trip() {
    let credential = CredentialRef::parse("cred_mcp_test").expect("credential reference");
    let mut env = std::collections::BTreeMap::new();
    env.insert("MCP_ENDPOINT".to_owned(), "https://example.test".to_owned());
    let server = McpServerSetting {
        mcp_revision: "mcp_files".to_owned(),
        name: "Files".to_owned(),
        transport: "stdio".to_owned(),
        endpoint: "mcp-server".to_owned(),
        protocol_version: "2025-06-18".to_owned(),
        args: vec!["--stdio".to_owned()],
        env,
        headers: std::collections::BTreeMap::new(),
        query_params: std::collections::BTreeMap::new(),
        auth: Some(McpAuthSetting {
            kind: McpAuthKind::Env,
            name: Some("MCP_TOKEN".to_owned()),
            credential_ref: Some(credential.clone()),
        }),
        credential_ref: None,
        enabled: true,
    };
    let document = SettingsDocument {
        active_profile_revision: Some("profile_main".to_owned()),
        profiles: vec![ProfileSetting {
            profile_revision: "profile_main".to_owned(),
            name: "Main".to_owned(),
            provider: "openai".to_owned(),
            protocol: ApiProtocol::OpenAiChatCompletions,
            model: "gpt-test".to_owned(),
            base_url: Some("https://api.example.test/v1".to_owned()),
            credential_ref: None,
            supports_vision: false,
            access_mode: AccessMode::ReadOnly,
            skill_revisions: vec!["skill_default".to_owned()],
            mcp_revisions: Some(vec!["mcp_files".to_owned()]),
        }],
        mcp_servers: vec![server],
        ..SettingsDocument::default()
    };
    document.validate().expect("structured settings validate");
    let encoded = serde_json::to_vec(&document).expect("settings json");
    let decoded: SettingsDocument = serde_json::from_slice(&encoded).expect("settings decode");
    assert_eq!(decoded, document);
    let encoded_text = String::from_utf8(encoded.clone()).expect("settings utf8");
    assert!(encoded_text.contains(r#""protocol":"openai_chat_completions""#));
    assert!(!encoded_text.contains("open_ai_chat_completions"));
    let legacy_text = encoded_text.replace("openai_chat_completions", "open_ai_chat_completions");
    let legacy_decoded: SettingsDocument =
        serde_json::from_str(&legacy_text).expect("legacy settings protocol alias");
    assert_eq!(legacy_decoded, document);
    assert_eq!(decoded.profiles[0].access_mode, AccessMode::ReadOnly);
    assert_eq!(
        decoded.profiles[0]
            .mcp_revisions
            .as_ref()
            .expect("mcp selection"),
        &vec!["mcp_files".to_owned()]
    );
}

/// The canonical OpenAI spelling must be emitted while the pre-fix spelling
/// remains readable so existing settings do not become invalid on upgrade.
#[test]
fn api_protocol_uses_canonical_openai_wire_name_and_reads_legacy_alias() {
    let canonical = serde_json::to_string(&ApiProtocol::OpenAiChatCompletions)
        .expect("canonical protocol json");
    assert_eq!(canonical, r#""openai_chat_completions""#);

    let legacy = serde_json::from_str::<ApiProtocol>(r#""open_ai_chat_completions""#)
        .expect("legacy protocol alias");
    assert_eq!(legacy, ApiProtocol::OpenAiChatCompletions);

    let responses =
        serde_json::to_string(&ApiProtocol::OpenAiResponses).expect("responses protocol json");
    assert_eq!(responses, r#""openai_responses""#);
    let legacy_responses = serde_json::from_str::<ApiProtocol>(r#""open_ai_responses""#)
        .expect("legacy responses alias");
    assert_eq!(legacy_responses, ApiProtocol::OpenAiResponses);
}

/// Responses remains a recognized wire enum for migration, but settings
/// validation keeps it unavailable until the product implements that API.
#[test]
fn canonical_responses_protocol_remains_unavailable_to_settings() {
    let mut document = SettingsDocument::default();
    document.profiles.push(ProfileSetting {
        profile_revision: "profile_responses_canonical".to_owned(),
        name: "Reserved".to_owned(),
        provider: "openai".to_owned(),
        protocol: serde_json::from_str(r#""openai_responses""#).expect("canonical responses"),
        model: "model".to_owned(),
        base_url: Some("https://example.com/v1".to_owned()),
        credential_ref: None,
        supports_vision: false,
        access_mode: Default::default(),
        skill_revisions: Vec::new(),
        mcp_revisions: None,
    });
    assert!(matches!(
        document.validate(),
        Err(super::model::SettingsModelError::UnsupportedProtocol)
    ));
}

#[test]
fn credential_backend_only_returns_a_runtime_delivery_object() {
    let backend = FakeBackend::default();
    let vault = CredentialVault::new(backend.clone());
    let reference = CredentialRef::parse("cred_test").expect("reference");
    vault
        .set(CredentialPurpose::Model, &reference, "secret-value")
        .expect("set");
    let delivery = vault
        .resolve(CredentialPurpose::Model, &reference)
        .expect("resolve");
    assert!(!format!("{delivery:?}").contains("secret-value"));
    let mut channel = CaptureChannel::default();
    delivery.deliver(&mut channel).expect("deliver");
    assert_eq!(channel.bytes, b"secret-value");
}

#[test]
fn serde_rejects_a_non_opaque_credential_reference() {
    let result = serde_json::from_str::<CredentialRef>(r#""api-key""#);
    assert!(result.is_err());
}

#[cfg(windows)]
#[test]
fn native_keyring_probe_reads_only_a_random_missing_reference() {
    let backend = NativeKeyringBackend;
    let reference =
        CredentialRef::parse(&format!("cred_probe_{}", uuid::Uuid::new_v4())).expect("reference");
    let result = backend.get(CredentialPurpose::Model, &reference);
    assert!(matches!(
        result,
        Err(SecretError::NotFound) | Err(SecretError::StoreUnavailable)
    ));
}

#[derive(Clone, Default)]
struct FakeBackend {
    values: Arc<Mutex<HashMap<String, String>>>,
}

impl SecretBackend for FakeBackend {
    fn set(
        &self,
        _purpose: CredentialPurpose,
        reference: &CredentialRef,
        secret: &str,
    ) -> Result<(), SecretError> {
        self.values
            .lock()
            .expect("fake lock")
            .insert(reference.as_str().to_owned(), secret.to_owned());
        Ok(())
    }

    fn get(
        &self,
        _purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<RuntimeSecretDelivery, SecretError> {
        let secret = self
            .values
            .lock()
            .expect("fake lock")
            .get(reference.as_str())
            .cloned()
            .ok_or(SecretError::NotFound)?;
        RuntimeSecretDelivery::new(secret)
    }

    fn delete(
        &self,
        _purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<(), SecretError> {
        self.values
            .lock()
            .expect("fake lock")
            .remove(reference.as_str());
        Ok(())
    }
}

#[derive(Default)]
struct CaptureChannel {
    bytes: Vec<u8>,
}

impl SecretDeliveryChannel for CaptureChannel {
    fn send_secret(&mut self, secret: &[u8]) -> Result<(), SecretDeliveryError> {
        self.bytes.extend_from_slice(secret);
        Ok(())
    }
}
