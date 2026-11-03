//! Configured secret values, registered once at boot and replaced in every log
//! sink (console, daily file, run logs, alert pushes) and MCP tool error. It
//! catches a secret that reaches text through a path nobody redacted, such as
//! an upstream error echoing a request.

use std::borrow::Cow;
use std::sync::RwLock;

/// Shorter values could match ordinary words, and no credential is this short.
pub const MIN_SECRET_CHARS: usize = 8;
pub const REDACTED: &str = "[redacted]";

static SECRETS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Adds values to scrub; blank and short values are ignored. Longer values are
/// replaced first so one secret containing another is removed whole.
pub fn register<I: IntoIterator<Item = String>>(values: I) {
    let mut secrets = SECRETS
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for value in values {
        let value = value.trim().to_owned();
        if value.chars().count() >= MIN_SECRET_CHARS && !secrets.contains(&value) {
            secrets.push(value);
        }
    }
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
}

/// `text` with every registered secret replaced by [`REDACTED`].
pub fn scrub(text: &str) -> Cow<'_, str> {
    let secrets = SECRETS
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out = Cow::Borrowed(text);
    for secret in secrets.iter() {
        if out.contains(secret.as_str()) {
            out = Cow::Owned(out.replace(secret.as_str(), REDACTED));
        }
    }
    out
}

/// [`scrub`] for an owned string, reusing it when nothing matched.
pub fn scrub_owned(text: String) -> String {
    match scrub(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(clean) => clean,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_registered_values_and_ignores_short_ones() {
        register([
            "secret-value-1234".to_owned(),
            "secret-value-1234-longer".to_owned(),
            "short".to_owned(),
            "   ".to_owned(),
        ]);
        assert_eq!(
            scrub("a secret-value-1234-longer b secret-value-1234 c short"),
            "a [redacted] b [redacted] c short"
        );
        assert!(matches!(scrub("nothing here"), Cow::Borrowed(_)));
        assert_eq!(
            scrub_owned("x secret-value-1234".to_owned()),
            "x [redacted]"
        );
    }
}
