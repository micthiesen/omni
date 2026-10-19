//! The outbox's storage key and what derives from it.
//!
//! The key lives in `mcp-events.key` in the data directory ([`load_event_key`]),
//! never in the database. Before `/mcp` was removed it was derived from
//! `OMNI_MCP_TOKEN`; the first boot after that migrates exactly that key into
//! the file, so stored rows keep their key id and subscription ids.
//!
//! - legacy storage key: `HMAC-SHA256(token, "omni-mcp-events-storage-v1")`;
//! - key id: `sha256hex(base64(key))`;
//! - sealed values: `base64(nonce12 | tag16 | ciphertext)` with AES-256-GCM;
//! - subscription id: `sub_` + the first 40 hex digits of
//!   `HMAC-SHA256(key, JSON.stringify([owner, url, name, canonicalArgs]))`.
//!
//! Replacing the key therefore invalidates every stored credential.

use aes_gcm::aead::{Aead as _, KeyInit as _};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac as _};
use serde_json::json;
use sha2::Sha256;

use super::catalog::{EventArguments, canonical_event_arguments};
use crate::gateway::secrets::{SecretsError, load_or_create_key_file, random_key};

/// The outbox key file in the data directory (base64url, mode 0600).
pub const EVENT_KEY_FILE: &str = "mcp-events.key";
/// The environment variable the key is migrated from once, then never read.
pub const LEGACY_TOKEN_VAR: &str = "OMNI_MCP_TOKEN";

/// How [`load_event_key`] obtained the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    File,
    MigratedFromToken,
    Generated,
}

/// The storage key the previous version derived from the MCP token.
pub fn legacy_token_key(token: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    let derived = hmac_sha256(token.as_bytes(), b"omni-mcp-events-storage-v1");
    key.copy_from_slice(&derived[..32.min(derived.len())]);
    key
}

/// The outbox key: `data_dir/mcp-events.key` when it exists; otherwise the
/// key derived from `legacy_token` (the old `OMNI_MCP_TOKEN`) when given, else
/// a random one, persisted to the file either way.
pub fn load_event_key(
    data_dir: &std::path::Path,
    legacy_token: Option<&str>,
) -> Result<([u8; 32], KeySource), SecretsError> {
    let path = data_dir.join(EVENT_KEY_FILE);
    let existed = path.exists();
    let legacy = legacy_token.filter(|token| !token.is_empty());
    let initial = || legacy.map_or_else(random_key, legacy_token_key);
    let key = load_or_create_key_file(&path, &initial)?;
    let source = match (existed, legacy) {
        (true, _) => KeySource::File,
        (false, Some(_)) => KeySource::MigratedFromToken,
        (false, None) => KeySource::Generated,
    };
    Ok((key, source))
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(message);
            mac.finalize().into_bytes().to_vec()
        }
        // HMAC accepts keys of any length.
        Err(_) => Vec::new(),
    }
}

/// The key material of one process.
#[derive(Clone)]
pub struct EventCrypto {
    key: [u8; 32],
    key_id: String,
}

impl std::fmt::Debug for EventCrypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCrypto")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl EventCrypto {
    /// The key derived from `secret` as the previous version derived it from
    /// the MCP token (tests and fixtures).
    pub fn new(secret: &str) -> Self {
        Self::from_key(legacy_token_key(secret))
    }

    pub fn from_key(key: [u8; 32]) -> Self {
        let key_id = omni_core::digest::sha256_hex(STANDARD.encode(key));
        Self { key, key_id }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn seal(&self, value: &str) -> String {
        let mut nonce = [0u8; 12];
        rand::fill(&mut nonce);
        let sealed = Aes256Gcm::new_from_slice(&self.key)
            .ok()
            .and_then(|cipher| cipher.encrypt(&Nonce::from(nonce), value.as_bytes()).ok())
            .unwrap_or_default();
        // aes-gcm appends the tag; the stored layout puts it before the ciphertext.
        let split = sealed.len().saturating_sub(16);
        let (ciphertext, tag) = sealed.split_at(split);
        let mut out = Vec::with_capacity(12 + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(tag);
        out.extend_from_slice(ciphertext);
        STANDARD.encode(out)
    }

    /// The plaintext, or `None` for a value sealed with another key.
    pub fn open(&self, value: &str) -> Option<String> {
        let bytes = STANDARD.decode(value).ok()?;
        if bytes.len() < 28 {
            return None;
        }
        let nonce = Nonce::try_from(&bytes[..12]).ok()?;
        let mut sealed = bytes[28..].to_vec();
        sealed.extend_from_slice(&bytes[12..28]);
        let cipher = Aes256Gcm::new_from_slice(&self.key).ok()?;
        let plain = cipher.decrypt(&nonce, sealed.as_slice()).ok()?;
        String::from_utf8(plain).ok()
    }

    pub fn subscription_id(
        &self,
        owner: &str,
        name: &str,
        arguments: &EventArguments,
        url: &str,
    ) -> String {
        let material = omni_core::js::json_stringify(&json!([
            owner,
            url,
            name,
            canonical_event_arguments(arguments)
        ]));
        let mac = hmac_sha256(&self.key, material.as_bytes());
        let hex = hex::encode(mac);
        format!("sub_{}", &hex[..40])
    }
}

/// A short, non-secret label that distinguishes clients. Owners from before
/// the gateway (Executor's delegated `executor:` owners and the `/mcp` token's
/// digest) are `legacy`.
pub fn owner_label(owner: &str) -> String {
    match owner.strip_prefix(crate::gateway::access::OWNER_PREFIX) {
        // `gateway:client:<uuid>` or `gateway:grant:<id>`, shortened.
        Some(rest) => format!("gateway:{}", rest.chars().take(20).collect::<String>()),
        None => "legacy".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_b64(value: &str) -> Vec<u8> {
        STANDARD.decode(value).unwrap_or_default()
    }

    #[test]
    fn seal_round_trips_and_hides_plaintext() {
        let crypto = EventCrypto::new("test-omni-bearer");
        let sealed = crypto.seal("https://example.com/callback");
        assert!(!sealed.contains("example.com"));
        assert_eq!(decode_b64(&sealed).len(), 12 + 16 + 28);
        assert_eq!(
            crypto.open(&sealed).as_deref(),
            Some("https://example.com/callback")
        );
        assert_eq!(EventCrypto::new("rotated-token").open(&sealed), None);
        assert_eq!(crypto.open("not base64!"), None);
    }

    /// Sealed by node's `createCipheriv("aes-256-gcm", key, nonce=7×12)` with
    /// the same key derivation, so stored production values open.
    #[test]
    fn opens_values_sealed_by_node() {
        let crypto = EventCrypto::new("test-omni-bearer");
        let sealed =
            "BwcHBwcHBwcHBwcHLLq3nBPhv0baD8f5HK8dU/PlYXwILBxKyeLeTpxSfbWfnJC6USJWzU/xV3TMfkw=";
        assert_eq!(
            crypto.open(sealed).as_deref(),
            Some("https://hooks.example.com/x?y=1")
        );
    }

    /// Computed independently (Python `hmac`/`hashlib`) from the previous
    /// version's derivation, so production rows keep their key id.
    const LEGACY_KEY_ID: &str = "8a4b4c4e5436594bae8efe34ad2e6d6af7abd52ffc7601aad6b9cca0d7b71b77";
    const LEGACY_KEY_B64URL: &str = "es5q7f_1Q7yL03aMy47TZctxr6MLaov-0_rl9zBZwIU";

    fn key_file(dir: &std::path::Path) -> String {
        std::fs::read_to_string(dir.join(EVENT_KEY_FILE)).unwrap()
    }

    #[test]
    fn legacy_token_key_matches_the_previous_derivation() {
        let key = legacy_token_key("test-omni-bearer");
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key),
            LEGACY_KEY_B64URL
        );
        assert_eq!(EventCrypto::from_key(key).key_id(), LEGACY_KEY_ID);
        assert_eq!(EventCrypto::new("test-omni-bearer").key_id(), LEGACY_KEY_ID);
    }

    #[test]
    fn migrates_the_token_key_into_the_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let (key, source) = load_event_key(dir.path(), Some("test-omni-bearer")).unwrap();
        assert_eq!(source, KeySource::MigratedFromToken);
        assert_eq!(EventCrypto::from_key(key).key_id(), LEGACY_KEY_ID);
        assert_eq!(key_file(dir.path()), LEGACY_KEY_B64URL);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join(EVENT_KEY_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The token can then leave the environment, or even change.
        for token in [None, Some("another-token")] {
            let (again, source) = load_event_key(dir.path(), token).unwrap();
            assert_eq!(source, KeySource::File);
            assert_eq!(EventCrypto::from_key(again).key_id(), LEGACY_KEY_ID);
        }
        // Values sealed under the token-derived key still open.
        let sealed = EventCrypto::new("test-omni-bearer").seal("https://hooks.example.com/x");
        assert_eq!(
            EventCrypto::from_key(key).open(&sealed).as_deref(),
            Some("https://hooks.example.com/x")
        );
    }

    #[test]
    fn a_fresh_install_generates_and_keeps_a_random_key() {
        let dir = tempfile::tempdir().unwrap();
        let (key, source) = load_event_key(dir.path(), None).unwrap();
        assert_eq!(source, KeySource::Generated);
        assert_ne!(EventCrypto::from_key(key).key_id(), LEGACY_KEY_ID);
        let (again, source) = load_event_key(dir.path(), Some("")).unwrap();
        assert_eq!((again, source), (key, KeySource::File));
        let other = tempfile::tempdir().unwrap();
        assert_ne!(load_event_key(other.path(), None).unwrap().0, key);
    }

    #[test]
    fn identities_are_stable_digests() {
        let crypto = EventCrypto::new("test-omni-bearer");
        assert_eq!(crypto.key_id().len(), 64);
        let args = EventArguments::from([("folder".to_owned(), "inbox".to_owned())]);
        let owner = "gateway:client:abc";
        let id = crypto.subscription_id(owner, "email.received", &args, "https://a.example/x");
        assert!(id.starts_with("sub_"));
        assert_eq!(id.len(), 44);
        assert_eq!(
            id,
            crypto.subscription_id(owner, "email.received", &args, "https://a.example/x")
        );
        assert_eq!(owner_label(owner), "gateway:client:abc");
        assert_eq!(
            owner_label(&format!("executor:{}", "a".repeat(64))),
            "legacy"
        );
        assert_eq!(owner_label("abc"), "legacy");
    }
}
