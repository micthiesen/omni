//! Gateway-owned names.
//!
//! An integration id is `^[a-z][a-z0-9_]{0,29}$` without `__` and without a
//! trailing `_`. Its tools are served as `<id>__<tool>` and its events as
//! `<id>__<event>`. Omni's own tool and event names never contain `__`, so a
//! served name is unambiguous by construction, whatever the upstream calls
//! its tools. A tool name outside `[A-Za-z0-9_-]`, or one that would exceed
//! [`MAX_TOOL_NAME`], is rewritten deterministically: invalid characters
//! become `_`, the remainder is shortened, and six hex digits of the
//! original name's SHA-256 are appended, so it depends only on the integration
//! id and the upstream name.

/// Clients prefix server names (`mcp__omni__`) and OpenAI-style function
/// names stop at 64 characters, so served names stay well under that.
pub const MAX_TOOL_NAME: usize = 52;
pub const SEPARATOR: &str = "__";

/// Why an integration id is invalid, if it is.
pub fn integration_id_problem(id: &str) -> Option<&'static str> {
    let mut chars = id.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return Some("must start with a lowercase letter");
    }
    if id.len() > 30 {
        return Some("must be at most 30 characters");
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Some("may contain only lowercase letters, digits and underscores");
    }
    if id.contains(SEPARATOR) {
        return Some("must not contain two consecutive underscores");
    }
    if id.ends_with('_') {
        return Some("must not end with an underscore");
    }
    None
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The served name of `upstream`, a tool of integration `id`.
pub fn exposed_tool_name(id: &str, upstream: &str) -> String {
    let plain = format!("{id}{SEPARATOR}{upstream}");
    if !upstream.is_empty() && upstream.chars().all(is_name_char) && plain.len() <= MAX_TOOL_NAME {
        return plain;
    }
    let sanitized: String = upstream
        .chars()
        .map(|c| if is_name_char(c) { c } else { '_' })
        .collect();
    let suffix = &omni_core::digest::sha256_hex(upstream)[..6];
    let room = MAX_TOOL_NAME.saturating_sub(id.len() + SEPARATOR.len() + suffix.len() + 1);
    let stem: String = sanitized.chars().take(room).collect();
    format!("{id}{SEPARATOR}{stem}_{suffix}")
}

/// The served name of an integration event.
pub fn exposed_event_name(id: &str, upstream: &str) -> String {
    format!("{id}{SEPARATOR}{upstream}")
}

/// The integration id a served name belongs to, if it is an integration's.
pub fn integration_of(name: &str) -> Option<&str> {
    name.split_once(SEPARATOR)
        .map(|(id, _)| id)
        .filter(|id| integration_id_problem(id).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integration_ids() {
        for ok in ["github", "artifacts_support", "a", "serpapi_mcp", "x1"] {
            assert_eq!(integration_id_problem(ok), None, "{ok}");
        }
        for bad in [
            "",
            "Github",
            "1abc",
            "a__b",
            "abc_",
            "a-b",
            "a.b",
            &"a".repeat(31),
        ] {
            assert!(integration_id_problem(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn plain_names_are_prefixed() {
        assert_eq!(
            exposed_tool_name("github", "get_issue"),
            "github__get_issue"
        );
        assert_eq!(
            exposed_tool_name("artifacts", "artifact-read"),
            "artifacts__artifact-read"
        );
        assert_eq!(integration_of("github__get_issue"), Some("github"));
        assert_eq!(integration_of("email_search"), None);
    }

    #[test]
    fn invalid_and_long_names_are_rewritten_deterministically() {
        let dotted = exposed_tool_name("ctx", "docs.search");
        assert!(dotted.starts_with("ctx__docs_search_"), "{dotted}");
        assert_eq!(dotted, exposed_tool_name("ctx", "docs.search"));
        // A sibling whose plain name equals the sanitized one stays distinct.
        assert_ne!(dotted, exposed_tool_name("ctx", "docs_search"));
        let long = "x".repeat(80);
        let served = exposed_tool_name("artifacts_support", &long);
        assert!(served.len() <= MAX_TOOL_NAME, "{served}");
        assert_ne!(
            served,
            exposed_tool_name("artifacts_support", &"x".repeat(81))
        );
        assert!(served.chars().all(is_name_char));
        assert!(exposed_tool_name("a", "").starts_with("a___"));
    }
}
