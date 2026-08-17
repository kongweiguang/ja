// SPDX-License-Identifier: GPL-3.0-or-later
// @author kongweiguang

//! Native credential storage and the non-serializable runtime delivery seam.

use super::model::CredentialRef;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{Duration, Instant};

const KEYRING_SERVICE: &str = "io.github.kongweiguang.ja";
const MAX_SECRET_BYTES: usize = 1_048_576;
const DELIVERY_TTL: Duration = Duration::from_secs(60);

/// The purpose is closed because it also controls the keychain account
/// namespace; arbitrary purpose strings must not become credential selectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialPurpose {
    Model,
    Mcp,
}

impl CredentialPurpose {
    /// Separates model and MCP accounts so a reference cannot cross a secret
    /// purpose boundary by changing only a caller-side enum.
    fn as_key_prefix(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Mcp => "mcp",
        }
    }
}

/// Stable categories intentionally omit the platform error text, which may
/// contain account names or other native credential-store diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SecretError {
    InvalidReference,
    EmptySecret,
    SecretTooLarge,
    NotFound,
    StoreUnavailable,
    DeliveryExpired,
    DeliveryFailed,
}

impl fmt::Display for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidReference => "invalid credential reference",
            Self::EmptySecret => "empty secret",
            Self::SecretTooLarge => "secret exceeds size limit",
            Self::NotFound => "credential not found",
            Self::StoreUnavailable => "credential store unavailable",
            Self::DeliveryExpired => "secret delivery expired",
            Self::DeliveryFailed => "secret delivery failed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SecretError {}

/// A runtime channel is deliberately narrower than `Write`: only the sidecar
/// delivery implementation should receive secret bytes, not a serializable
/// DTO or a general-purpose log/file writer.
pub trait SecretDeliveryChannel {
    fn send_secret(&mut self, secret: &[u8]) -> Result<(), SecretDeliveryError>;
}

/// Delivery failures remain static so the provider secret cannot be included
/// in an error string produced by a future transport implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretDeliveryError {
    Closed,
    Rejected,
    Io,
}

impl fmt::Display for SecretDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Closed => "delivery channel closed",
            Self::Rejected => "delivery channel rejected secret",
            Self::Io => "delivery channel I/O failed",
        })
    }
}

impl std::error::Error for SecretDeliveryError {}

/// The secret is private, has no Serialize/Clone/Display implementation, and
/// is zeroed when dropped.  The only public operation is consuming delivery
/// into the narrow runtime channel above.
pub struct RuntimeSecretDelivery {
    bytes: Vec<u8>,
    expires_at: Instant,
}

impl RuntimeSecretDelivery {
    /// Creates a bounded, expiring capability so a resolved credential cannot
    /// remain available indefinitely in the Java launch path.
    pub(crate) fn new(secret: String) -> Result<Self, SecretError> {
        if secret.is_empty() {
            return Err(SecretError::EmptySecret);
        }
        if secret.len() > MAX_SECRET_BYTES {
            return Err(SecretError::SecretTooLarge);
        }
        Ok(Self {
            bytes: secret.into_bytes(),
            expires_at: Instant::now() + DELIVERY_TTL,
        })
    }

    /// Consumes the delivery object so a caller cannot retain a second copy
    /// after the sidecar channel accepts the secret.
    pub fn deliver(self, channel: &mut dyn SecretDeliveryChannel) -> Result<(), SecretError> {
        if Instant::now() >= self.expires_at {
            return Err(SecretError::DeliveryExpired);
        }
        channel
            .send_secret(&self.bytes)
            .map_err(|_| SecretError::DeliveryFailed)
    }
}

impl fmt::Debug for RuntimeSecretDelivery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RuntimeSecretDelivery(REDACTED)")
    }
}

impl Drop for RuntimeSecretDelivery {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

/// Backend abstraction lets tests exercise malformed references and delivery
/// policy without writing credentials to a developer's real OS keychain.
pub trait SecretBackend: Send + Sync {
    fn set(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
        secret: &str,
    ) -> Result<(), SecretError>;
    fn get(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<RuntimeSecretDelivery, SecretError>;
    fn delete(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<(), SecretError>;
}

/// The production backend delegates to Windows Credential Manager or Apple
/// Keychain through the mature `keyring` crate; there is no JSON fallback.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeKeyringBackend;

impl SecretBackend for NativeKeyringBackend {
    fn set(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
        secret: &str,
    ) -> Result<(), SecretError> {
        validate_secret_input(reference, secret)?;
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account_name(purpose, reference))
            .map_err(|_| SecretError::StoreUnavailable)?;
        entry
            .set_password(secret)
            .map_err(|_| SecretError::StoreUnavailable)
    }

    fn get(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<RuntimeSecretDelivery, SecretError> {
        validate_reference(reference)?;
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account_name(purpose, reference))
            .map_err(|_| SecretError::StoreUnavailable)?;
        let secret = entry.get_password().map_err(|error| match error {
            keyring::Error::NoEntry => SecretError::NotFound,
            _ => SecretError::StoreUnavailable,
        })?;
        RuntimeSecretDelivery::new(secret)
    }

    fn delete(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<(), SecretError> {
        validate_reference(reference)?;
        let entry = keyring::Entry::new(KEYRING_SERVICE, &account_name(purpose, reference))
            .map_err(|_| SecretError::StoreUnavailable)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(SecretError::StoreUnavailable),
        }
    }
}

/// The vault is the only public façade used by settings/profile services;
/// callers receive an opaque delivery object rather than a `String` secret.
pub struct CredentialVault<B: SecretBackend> {
    backend: B,
}

impl<B: SecretBackend> CredentialVault<B> {
    /// Keeps backend selection outside UI code so secrets never become a
    /// serializable settings value.
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Stores a secret through the OS-keyring backend without returning its
    /// bytes to the caller.
    pub fn set(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
        secret: &str,
    ) -> Result<(), SecretError> {
        self.backend.set(purpose, reference, secret)
    }

    /// Resolves only a short-lived delivery capability for the sidecar, not a
    /// `String` that React or ordinary application state could retain.
    pub fn resolve(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<RuntimeSecretDelivery, SecretError> {
        self.backend.get(purpose, reference)
    }

    /// Deletes the opaque keyring entry while keeping the provider reference
    /// usable for a later explicit credential replacement.
    pub fn delete(
        &self,
        purpose: CredentialPurpose,
        reference: &CredentialRef,
    ) -> Result<(), SecretError> {
        self.backend.delete(purpose, reference)
    }
}

/// Revalidates serde-created references before they reach the native keychain
/// account namespace.
fn validate_reference(reference: &CredentialRef) -> Result<(), SecretError> {
    CredentialRef::parse(reference.as_str())
        .map(|_| ())
        .map_err(|_| SecretError::InvalidReference)
}

/// Enforces bounded non-empty input before handing bytes to the OS keychain.
fn validate_secret_input(reference: &CredentialRef, secret: &str) -> Result<(), SecretError> {
    validate_reference(reference)?;
    if secret.is_empty() {
        return Err(SecretError::EmptySecret);
    }
    if secret.len() > MAX_SECRET_BYTES {
        return Err(SecretError::SecretTooLarge);
    }
    Ok(())
}

/// Creates a stable account key from an opaque reference without embedding a
/// provider URL, model name, or secret value.
fn account_name(purpose: CredentialPurpose, reference: &CredentialRef) -> String {
    format!("{}:{}", purpose.as_key_prefix(), reference.as_str())
}
