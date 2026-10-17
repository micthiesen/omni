//! The running build's identity ([`BuildIdentity`]), computed once at boot.
//!
//! - `server`: the image revision from `OMNI_IMAGE_REVISION` (the Dockerfile
//!   sets it from the `OMNI_REVISION` build arg, which CI fills with the
//!   commit), else a fingerprint of the running executable's size and
//!   modification time, so a local rebuild also reads as a new server.
//! - `frontend`: a hash of the served `index.html`. Trunk names every asset by
//!   its content hash, so any frontend change rewrites that file.

use std::path::Path;
use std::time::UNIX_EPOCH;

use omni_api::build::BuildIdentity;
use omni_core::digest::sha256_hex;

/// The env var the Docker image sets from its `OMNI_REVISION` build arg.
pub const REVISION_ENV: &str = "OMNI_IMAGE_REVISION";
/// Hex characters kept from each identity.
const ID_LEN: usize = 12;
/// Either part when it cannot be determined.
pub const UNKNOWN: &str = "unknown";

/// The identity of this process serving `web_dist`.
pub fn current(web_dist: &Path) -> BuildIdentity {
    BuildIdentity {
        server: server_identity(std::env::var(REVISION_ENV).ok().as_deref()),
        frontend: frontend_identity(web_dist),
    }
}

/// `revision` (trimmed and shortened) when set, else the executable fingerprint.
pub fn server_identity(revision: Option<&str>) -> String {
    match revision.map(str::trim).filter(|r| !r.is_empty()) {
        Some(revision) => revision.chars().take(ID_LEN).collect(),
        None => executable_fingerprint().unwrap_or_else(|| UNKNOWN.to_owned()),
    }
}

fn executable_fingerprint() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(&exe).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some(short_hash(
        format!("{}\n{}\n{modified}", exe.display(), meta.len()).as_bytes(),
    ))
}

/// A hash of `web_dist/index.html`, or [`UNKNOWN`] when it is missing.
pub fn frontend_identity(web_dist: &Path) -> String {
    std::fs::read(web_dist.join("index.html"))
        .map_or_else(|_| UNKNOWN.to_owned(), |html| short_hash(&html))
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hex = sha256_hex(bytes);
    hex.truncate(ID_LEN);
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_identity_follows_index_html() {
        let dist = tempfile::tempdir().unwrap();
        assert_eq!(frontend_identity(dist.path()), UNKNOWN);
        let index = dist.path().join("index.html");
        std::fs::write(&index, r#"<script src="/app-1a2b.js"></script>"#).unwrap();
        let first = frontend_identity(dist.path());
        assert_eq!(first.len(), ID_LEN);
        assert_eq!(
            frontend_identity(dist.path()),
            first,
            "stable while unchanged"
        );
        std::fs::write(&index, r#"<script src="/app-3c4d.js"></script>"#).unwrap();
        assert_ne!(frontend_identity(dist.path()), first);
    }

    #[test]
    fn server_identity_prefers_the_image_revision() {
        assert_eq!(
            server_identity(Some(" 16df70d4c0ffee1234567890 ")),
            "16df70d4c0ff"
        );
        let fallback = server_identity(None);
        assert_ne!(fallback, UNKNOWN, "the test binary has a fingerprint");
        assert_eq!(server_identity(Some("")), fallback);
        assert_eq!(server_identity(None), fallback, "stable per executable");
    }
}
