//! Keys and identities derived from the MCP bearer token (never stored).
//!
//! - storage key: `HMAC-SHA256(token, "omni-mcp-events-storage-v1")`;
//! - key id: `sha256hex(base64(key))`;
//! - direct owner: `sha256hex("omni-mcp-events-owner-v1:" + token)`;
//! - sealed values: `base64(nonce12 | tag16 | ciphertext)` with AES-256-GCM;
//! - subscription id: `sub_` + the first 40 hex digits of
//!   `HMAC-SHA256(key, JSON.stringify([owner, url, name, canonicalArgs]))`.
//!
//! Rotating the token therefore invalidates every stored credential.

use aes_gcm::aead::{Aead as _, KeyInit as _};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, Mac as _};
use serde_json::json;
use sha2::Sha256;

use super::catalog::{EventArguments, canonical_event_arguments};

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

/// The token-derived key material of one process.
#[derive(Clone)]
pub struct EventCrypto {
    key: [u8; 32],
    key_id: String,
    owner: String,
}

impl std::fmt::Debug for EventCrypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCrypto")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl EventCrypto {
    pub fn new(token: &str) -> Self {
        let mut key = [0u8; 32];
        let derived = hmac_sha256(token.as_bytes(), b"omni-mcp-events-storage-v1");
        key.copy_from_slice(&derived[..32.min(derived.len())]);
        let key_id = omni_core::digest::sha256_hex(STANDARD.encode(key));
        let owner = omni_core::digest::sha256_hex(format!("omni-mcp-events-owner-v1:{token}"));
        Self { key, key_id, owner }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The owner of subscriptions made with the MCP token itself.
    pub fn direct_owner(&self) -> &str {
        &self.owner
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

/// A short, non-secret label that distinguishes delegated clients.
pub fn owner_label(owner: &str) -> String {
    if owner.starts_with("executor:") {
        owner.chars().take(21).collect()
    } else if let Some(rest) = owner.strip_prefix(crate::gateway::access::OWNER_PREFIX) {
        // `gateway:client:<uuid>` or `gateway:grant:<id>`, shortened.
        format!("gateway:{}", rest.chars().take(20).collect::<String>())
    } else {
        "direct".to_owned()
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

    #[test]
    fn identities_are_stable_digests() {
        let crypto = EventCrypto::new("test-omni-bearer");
        assert_eq!(crypto.key_id().len(), 64);
        assert_eq!(crypto.direct_owner().len(), 64);
        let args = EventArguments::from([("folder".to_owned(), "inbox".to_owned())]);
        let id = crypto.subscription_id(
            crypto.direct_owner(),
            "email.received",
            &args,
            "https://a.example/x",
        );
        assert!(id.starts_with("sub_"));
        assert_eq!(id.len(), 44);
        assert_eq!(
            id,
            crypto.subscription_id(
                crypto.direct_owner(),
                "email.received",
                &args,
                "https://a.example/x"
            )
        );
        assert_eq!(
            owner_label(&format!("executor:{}", "a".repeat(64))),
            format!("executor:{}", "a".repeat(12))
        );
        assert_eq!(owner_label("abc"), "direct");
    }
}
