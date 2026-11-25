//! Random identifiers.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// `crypto.randomUUID()`: lowercase hyphenated UUID v4.
pub fn uuid_v4() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `randomBytes(bytes).toString("base64url")` from the OS CSPRNG.
pub fn secure_id_b64url(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    rand::fill(buffer.as_mut_slice());
    URL_SAFE_NO_PAD.encode(&buffer)
}

/// Characters people read off one screen and type on another: no `0/O`,
/// `1/I/L` or other look-alikes.
pub const READABLE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// `len` characters from [`READABLE_ALPHABET`] (5 bits each) from the OS CSPRNG.
pub fn readable_code(len: usize) -> String {
    let mut buffer = vec![0u8; len];
    rand::fill(buffer.as_mut_slice());
    buffer
        .iter()
        .map(|b| char::from(READABLE_ALPHABET[usize::from(b & 31)]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_expected_shape() {
        let id = uuid_v4();
        assert_eq!(id.len(), 36);
        assert_eq!(id.as_bytes()[14], b'4');
        assert_eq!(secure_id_b64url(32).len(), 43);
        assert_ne!(secure_id_b64url(16), secure_id_b64url(16));
        let code = readable_code(8);
        assert_eq!(code.len(), 8);
        assert!(code.bytes().all(|b| READABLE_ALPHABET.contains(&b)));
    }
}
