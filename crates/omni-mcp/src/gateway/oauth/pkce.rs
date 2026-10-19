//! PKCE (RFC 7636), S256 only.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;

/// The length of an S256 challenge: base64url (no padding) of 32 bytes.
pub const CHALLENGE_LEN: usize = 43;

/// A new verifier: 32 random bytes, base64url without padding (43 chars).
pub fn verifier() -> String {
    omni_core::ids::secure_id_b64url(32)
}

/// `BASE64URL(SHA256(verifier))`, the S256 challenge.
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 43 to 128 characters of the unreserved set `[A-Za-z0-9-._~]`.
pub fn is_valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// The shape of an S256 challenge: 43 base64url characters.
pub fn is_valid_challenge(challenge: &str) -> bool {
    challenge.len() == CHALLENGE_LEN
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Whether `verifier` is well formed and hashes to `challenge` (constant time).
pub fn verify(verifier: &str, challenge: &str) -> bool {
    if !is_valid_verifier(verifier) {
        return false;
    }
    let expected = self::challenge(verifier);
    expected.len() == challenge.len() && bool::from(expected.as_bytes().ct_eq(challenge.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_7636_appendix_b_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(verify(
            verifier,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        ));
        assert!(!verify(
            verifier,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cN"
        ));
    }

    #[test]
    fn verifiers_are_well_formed_and_distinct() {
        let a = verifier();
        assert_eq!(a.len(), 43);
        assert!(is_valid_verifier(&a));
        assert_ne!(a, verifier());
        assert!(is_valid_challenge(&challenge(&a)));
        assert!(verify(&a, &challenge(&a)));
    }

    #[test]
    fn malformed_verifiers_never_verify() {
        let short = "a".repeat(42);
        assert!(!is_valid_verifier(&short));
        assert!(!verify(&short, &challenge(&short)));
        let long = "a".repeat(129);
        assert!(!verify(&long, &challenge(&long)));
        let spaced = format!("{} ", "a".repeat(43));
        assert!(!verify(&spaced, &challenge(&spaced)));
        assert!(is_valid_verifier(&"a~.-_".repeat(10)));
        assert!(!is_valid_challenge("short"));
        assert!(!is_valid_challenge(&"+".repeat(43)));
    }
}
