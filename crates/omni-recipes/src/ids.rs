//! Record ids: a prefix and eight lowercase base32 characters
//! (`rcp_7h2k9m4x`).

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

pub const RECIPE: &str = "rcp_";
pub const CHANGE: &str = "chg_";
pub const INBOX: &str = "inb_";
pub const COOK: &str = "cook_";
pub const IMAGE: &str = "img_";
/// Chosen by the app, like a cook session's id.
pub const REQUEST: &str = "req_";

/// A new random id with `prefix`.
pub fn new_id(prefix: &str) -> String {
    let random = omni_core::ids::uuid_v4();
    let mut id = String::with_capacity(prefix.len() + 8);
    id.push_str(prefix);
    let bytes = random.as_bytes().iter().filter(|b| **b != b'-');
    // Each hex digit pair of the UUID gives eight random bits; five are used.
    let mut pending: Option<u8> = None;
    let mut count = 0;
    for byte in bytes {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => 0,
        };
        match pending.take() {
            None => pending = Some(nibble),
            Some(high) => {
                let value = (high << 4) | nibble;
                id.push(char::from(ALPHABET[usize::from(value & 31)]));
                count += 1;
                if count == 8 {
                    break;
                }
            }
        }
    }
    id
}

/// Whether `id` is `prefix` plus eight lowercase letters or digits.
pub fn is_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix).is_some_and(|rest| {
        rest.len() == 8
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    })
}

/// A client-chosen token: 1 to 64 letters, digits, `_` or `-`.
pub fn is_client_token(token: &str) -> bool {
    (1..=64).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_have_the_prefix_and_eight_base32_characters() {
        for prefix in [RECIPE, CHANGE, INBOX, COOK, IMAGE] {
            let id = new_id(prefix);
            assert!(is_id(&id, prefix), "{id}");
            assert_ne!(id, new_id(prefix));
        }
        assert!(is_id("cook_2b7r9d4h", COOK));
        assert!(!is_id("cook_2B7r9d4h", COOK));
        assert!(!is_id("cook_2b7r9d4", COOK));
        assert!(!is_id("rcp_2b7r9d4h", COOK));
    }
}
