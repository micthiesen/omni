//! Gateway secret sealing.
//!
//! One random 256-bit key lives in a mode-0600 file in the data directory
//! (`gateway.key`, beside the database), so a copy of the database alone (the `prod_copy` workflow,
//! a support bundle) cannot reveal upstream credentials. Values are sealed
//! with AES-256-GCM as `v1.<nonce>.<ciphertext+tag>` (base64url), with the
//! row they belong to as associated data, so a sealed value moved to another
//! row does not open. Downstream access tokens are signed with a key derived
//! from the same file.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead as _, KeyInit as _, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac as _};
use sha2::Sha256;

const KEY_FILE: &str = "gateway.key";

#[derive(Debug, thiserror::Error)]
pub enum SecretsError {
    #[error("gateway key file {path}: {reason}")]
    KeyFile { path: PathBuf, reason: String },
}

/// The gateway's key material; cheap to clone.
#[derive(Clone)]
pub struct GatewaySecrets {
    key: [u8; 32],
}

impl std::fmt::Debug for GatewaySecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GatewaySecrets(<redacted>)")
    }
}

/// `gateway.key` in the data directory.
pub fn key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(KEY_FILE)
}

impl GatewaySecrets {
    /// Reads the key file, creating it (0600) on first use. The key is
    /// written to a temporary file and linked into place, so a crash never
    /// leaves a partial key; an existing key is never replaced.
    pub fn load_or_create(path: &Path) -> Result<Self, SecretsError> {
        let fail = |reason: String| SecretsError::KeyFile {
            path: path.to_owned(),
            reason,
        };
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let bytes = URL_SAFE_NO_PAD
                    .decode(text.trim())
                    .map_err(|_| fail("not base64url".to_owned()))?;
                let key: [u8; 32] = bytes
                    .try_into()
                    .map_err(|_| fail("not a 256-bit key".to_owned()))?;
                Ok(Self { key })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                    std::fs::create_dir_all(dir).map_err(|e| fail(e.to_string()))?;
                }
                let mut key = [0u8; 32];
                rand::fill(&mut key);
                let staging = path.with_extension(format!("tmp-{}", std::process::id()));
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;
                    options.mode(0o600);
                }
                let written = options.open(&staging).and_then(|mut file| {
                    file.write_all(URL_SAFE_NO_PAD.encode(key).as_bytes())?;
                    file.sync_all()
                });
                // A hard link fails if another process created the key first.
                let linked = written.and_then(|()| std::fs::hard_link(&staging, path));
                let _ = std::fs::remove_file(&staging);
                match linked {
                    Ok(()) => Ok(Self { key }),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        Self::load_or_create(path)
                    }
                    Err(error) => Err(fail(error.to_string())),
                }
            }
            Err(error) => Err(fail(error.to_string())),
        }
    }

    /// A fixed key (tests).
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Seals `plaintext` for the row named by `context`.
    pub fn seal(&self, context: &str, plaintext: &str) -> String {
        let mut nonce = [0u8; 12];
        rand::fill(&mut nonce);
        let sealed = Aes256Gcm::new_from_slice(&self.key)
            .ok()
            .and_then(|cipher| {
                cipher
                    .encrypt(
                        &Nonce::from(nonce),
                        Payload {
                            msg: plaintext.as_bytes(),
                            aad: context.as_bytes(),
                        },
                    )
                    .ok()
            })
            .unwrap_or_default();
        format!(
            "v1.{}.{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(sealed)
        )
    }

    /// Opens a value sealed for `context`; `None` when it does not verify.
    pub fn open(&self, context: &str, sealed: &str) -> Option<String> {
        let mut parts = sealed.split('.');
        if parts.next() != Some("v1") {
            return None;
        }
        let nonce: [u8; 12] = URL_SAFE_NO_PAD
            .decode(parts.next()?)
            .ok()?
            .try_into()
            .ok()?;
        let ciphertext = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?;
        if parts.next().is_some() {
            return None;
        }
        let plain = Aes256Gcm::new_from_slice(&self.key)
            .ok()?
            .decrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &ciphertext,
                    aad: context.as_bytes(),
                },
            )
            .ok()?;
        String::from_utf8(plain).ok()
    }

    /// `HMAC-SHA256(derived key, message)`, the signature of downstream access tokens.
    pub fn sign(&self, purpose: &str, message: &[u8]) -> [u8; 32] {
        let derived = hmac(&self.key, purpose.as_bytes());
        hmac(&derived, message)
    }
}

fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) {
        mac.update(message);
        out.copy_from_slice(&mac.finalize().into_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_values_open_only_for_their_row() {
        let secrets = GatewaySecrets::from_key([7; 32]);
        let sealed = secrets.seal("credential:github", "ghp_secret");
        assert!(!sealed.contains("ghp_secret"));
        assert_eq!(
            secrets.open("credential:github", &sealed).as_deref(),
            Some("ghp_secret")
        );
        assert_eq!(secrets.open("credential:serpapi", &sealed), None);
        assert_eq!(
            GatewaySecrets::from_key([8; 32]).open("credential:github", &sealed),
            None
        );
        assert_ne!(sealed, secrets.seal("credential:github", "ghp_secret"));
    }

    #[test]
    fn key_file_is_created_private_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(&dir.path().join("data"));
        let first = GatewaySecrets::load_or_create(&path).unwrap();
        let sealed = first.seal("row", "value");
        let again = GatewaySecrets::load_or_create(&path).unwrap();
        assert_eq!(again.open("row", &sealed).as_deref(), Some("value"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, "short").unwrap();
        assert!(GatewaySecrets::load_or_create(&path).is_err());
    }
}
