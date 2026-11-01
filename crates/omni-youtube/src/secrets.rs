//! Sealing of the stored refresh token.
//!
//! One random 256-bit key lives in `youtube.key` (mode 0600, beside the
//! database), so a copy of the database alone (the `prod_copy` workflow, a
//! support bundle) cannot reveal the token. Values are sealed with AES-256-GCM
//! as `v1.<nonce>.<ciphertext+tag>` (base64url) with the row as associated
//! data, the same format as the gateway's `gateway.key`.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead as _, KeyInit as _, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub const KEY_FILE: &str = "youtube.key";

#[derive(Debug, thiserror::Error)]
#[error("YouTube key file {path}: {reason}")]
pub struct KeyFileError {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone)]
pub struct Sealer {
    key: [u8; 32],
}

impl std::fmt::Debug for Sealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Sealer(<redacted>)")
    }
}

impl Sealer {
    /// Reads the key file, creating a random key (0600) on first use. The key
    /// is staged and hard-linked into place, so a crash never leaves a
    /// partial key and an existing key is never replaced.
    pub fn load_or_create(path: &Path) -> Result<Self, KeyFileError> {
        let fail = |reason: String| KeyFileError {
            path: path.to_owned(),
            reason,
        };
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let bytes = URL_SAFE_NO_PAD
                    .decode(text.trim())
                    .map_err(|_| fail("not base64url".to_owned()))?;
                let key = bytes
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

    /// `None` when the value does not verify for `context`.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_values_open_only_for_their_row() {
        let sealer = Sealer::from_key([7; 32]);
        let sealed = sealer.seal("youtube-account:owner", "1//refresh");
        assert!(!sealed.contains("refresh"));
        assert_eq!(
            sealer.open("youtube-account:owner", &sealed).as_deref(),
            Some("1//refresh")
        );
        assert_eq!(sealer.open("youtube-account:other", &sealed), None);
        assert_eq!(
            Sealer::from_key([8; 32]).open("youtube-account:owner", &sealed),
            None
        );
    }

    #[test]
    fn the_key_file_is_created_once_with_owner_only_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE);
        let first = Sealer::load_or_create(&path).unwrap();
        let sealed = first.seal("row", "value");
        let second = Sealer::load_or_create(&path).unwrap();
        assert_eq!(second.open("row", &sealed).as_deref(), Some("value"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
