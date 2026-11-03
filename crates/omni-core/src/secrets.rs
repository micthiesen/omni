//! Secret values replaced in every log sink (console, daily file, run logs,
//! alert pushes) and MCP tool error. It catches a secret that reaches text
//! through a path nobody redacted, such as an upstream error echoing a request.
//!
//! Configured secrets ([`register`]) stay for the process lifetime.
//! Credentials created or loaded at runtime ([`register_runtime`]) rotate, so
//! they live in a bounded first-in-first-out set: the oldest is forgotten once
//! [`MAX_RUNTIME_SECRETS`] newer ones arrive. Values live only in memory and
//! are never written anywhere.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::{PoisonError, RwLock};

/// Shorter values could match ordinary words, and no credential is this short.
pub const MIN_SECRET_CHARS: usize = 8;
/// Longer values are not credentials worth scanning every log line for.
pub const MAX_SECRET_BYTES: usize = 4096;
/// Runtime credentials remembered at once (each with its encoded forms).
pub const MAX_RUNTIME_SECRETS: usize = 256;
pub const REDACTED: &str = "[redacted]";

struct Registry {
    fixed: Vec<String>,
    /// One entry per registered credential: its raw and encoded forms.
    runtime: VecDeque<Vec<String>>,
    /// Every form, longest first (so a secret containing another is removed
    /// whole), rebuilt on registration so scrubbing a line never sorts.
    combined: Vec<String>,
}

impl Registry {
    fn rebuild(&mut self) {
        let mut combined: Vec<String> = self
            .fixed
            .iter()
            .chain(self.runtime.iter().flatten())
            .cloned()
            .collect();
        combined.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        combined.dedup();
        self.combined = combined;
    }
}

static REGISTRY: RwLock<Registry> = RwLock::new(Registry {
    fixed: Vec::new(),
    runtime: VecDeque::new(),
    combined: Vec::new(),
});

/// The forms a secret takes in text: raw, percent-encoded (URL path or query,
/// `%20` and `+` for spaces) and escaped (JSON strings, `{:?}` fields).
pub fn forms(value: &str) -> Vec<String> {
    let mut out = vec![value.to_owned()];
    let mut push = |form: String| {
        if !out.contains(&form) {
            out.push(form);
        }
    };
    push(percent_encode(value, false));
    push(percent_encode(value, true));
    push(value.escape_debug().to_string());
    out
}

fn percent_encode(value: &str, plus_for_space: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' if plus_for_space => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn usable(value: &str) -> Option<&str> {
    let value = value.trim();
    (value.chars().count() >= MIN_SECRET_CHARS && value.len() <= MAX_SECRET_BYTES).then_some(value)
}

/// Adds configured secrets (with their [`forms`]) for the process lifetime;
/// blank, short and oversized values are ignored.
pub fn register<I: IntoIterator<Item = String>>(values: I) {
    let mut registry = REGISTRY.write().unwrap_or_else(PoisonError::into_inner);
    for value in values {
        let Some(value) = usable(&value) else {
            continue;
        };
        for form in forms(value) {
            if !registry.fixed.contains(&form) {
                registry.fixed.push(form);
            }
        }
    }
    registry.rebuild();
}

/// A known value this close to the newest end is not moved again, so a
/// credential used on every request takes only the read lock.
const TOUCH_SLACK: usize = 32;

/// Adds a credential created or loaded at runtime (token, cookie, session
/// key). Registering a known value again marks it recently used, so a
/// long-lived credential re-registered at each load outlives rotating ones.
pub fn register_runtime(value: &str) {
    let Some(value) = usable(value) else {
        return;
    };
    let position = |registry: &Registry| {
        registry
            .runtime
            .iter()
            .position(|entry| entry.first().is_some_and(|raw| raw == value))
    };
    {
        let registry = REGISTRY.read().unwrap_or_else(PoisonError::into_inner);
        if position(&registry).is_some_and(|i| i + TOUCH_SLACK >= registry.runtime.len()) {
            return;
        }
    }
    let mut registry = REGISTRY.write().unwrap_or_else(PoisonError::into_inner);
    if let Some(i) = position(&registry) {
        // Same set of values, so `combined` needs no rebuild.
        if let Some(entry) = registry.runtime.remove(i) {
            registry.runtime.push_back(entry);
        }
        return;
    }
    if registry.runtime.len() >= MAX_RUNTIME_SECRETS {
        registry.runtime.pop_front();
    }
    registry.runtime.push_back(forms(value));
    registry.rebuild();
}

/// `text` with every registered secret replaced by [`REDACTED`].
pub fn scrub(text: &str) -> Cow<'_, str> {
    let registry = REGISTRY.read().unwrap_or_else(PoisonError::into_inner);
    let mut out = Cow::Borrowed(text);
    for secret in &registry.combined {
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

    // One test: the registry is process-global and these steps depend on order.
    #[test]
    fn scrubs_fixed_and_bounded_runtime_secrets_in_every_form() {
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

        register_runtime("tok/en+with \"quote\"");
        for text in [
            "raw tok/en+with \"quote\"",
            "url tok%2Fen%2Bwith%20%22quote%22",
            "form tok%2Fen%2Bwith+%22quote%22",
            r#"json "tok/en+with \"quote\"""#,
        ] {
            assert!(!scrub(text).contains("tok"), "{text} -> {}", scrub(text));
        }

        register_runtime("runtime-first-0000");
        for i in 1..=MAX_RUNTIME_SECRETS {
            register_runtime(&format!("runtime-rotated-{i:04}"));
        }
        let registry = REGISTRY.read().unwrap_or_else(PoisonError::into_inner);
        assert_eq!(registry.runtime.len(), MAX_RUNTIME_SECRETS);
        drop(registry);
        assert_eq!(scrub("runtime-first-0000"), "runtime-first-0000");
        assert_eq!(
            scrub(&format!("runtime-rotated-{MAX_RUNTIME_SECRETS:04}")),
            "[redacted]"
        );
        register_runtime(&format!("runtime-rotated-{MAX_RUNTIME_SECRETS:04}"));
        assert_eq!(REGISTRY.read().unwrap().runtime.len(), MAX_RUNTIME_SECRETS);

        // A long-lived credential re-registered at each use survives churn.
        register_runtime("long-lived-session");
        for round in 0..4 {
            for i in 0..MAX_RUNTIME_SECRETS / 2 {
                register_runtime(&format!("churn-{round}-{i:04}"));
            }
            register_runtime("long-lived-session");
        }
        assert_eq!(scrub("long-lived-session"), "[redacted]");
        assert_eq!(scrub("churn-0-0000"), "churn-0-0000");
    }
}
