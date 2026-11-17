//! URL identity for PressPods.
//!
//! Two submissions that point at the same article collapse to one episode.
//! Normalization is only for identity and dedup: the retrievers still fetch
//! the original URL. It strips known tracking params and the fragment,
//! ignores the scheme, lowercases the host, drops a leading `www.` and a
//! trailing DNS-root dot, sorts the remaining params and canonicalizes the
//! trailing slash. X status links collapse to their numeric post id.

use std::sync::LazyLock;

use regex::Regex;
use url::Url;

/// Query params that never affect which article a URL points at.
const TRACKING_PARAMS: &[&str] = &[
    "ref",
    "r",
    "fbclid",
    "gclid",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "igsh",
    "si",
    "triedRedirect",
    "source",
    "spm",
    "_hsenc",
    "_hsmi",
    "vero_id",
    "vero_conv",
    "oly_anon_id",
    "oly_enc_id",
    "s_cid",
    "cmpid",
    "ncid",
    "mkt_tok",
    "guccounter",
    "showWelcomeOnShare",
];

/// X/Twitter hosts whose status permalinks share one identity.
pub(crate) const X_HOSTS: &[&str] = &["x.com", "mobile.x.com", "twitter.com", "mobile.twitter.com"];

static X_STATUS_PATH: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/status|i/web/status)/(\d+)(?:/.*)?$").ok());

fn is_tracking_param(key: &str) -> bool {
    let lower = key.to_lowercase();
    lower.starts_with("utm_")
        || TRACKING_PARAMS.contains(&key)
        || TRACKING_PARAMS.contains(&lower.as_str())
}

/// JS string comparison (`a < b`): UTF-16 code unit order.
fn js_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// The numeric post id of an X status path, if `path` is one.
pub(crate) fn x_status_id(path: &str) -> Option<String> {
    X_STATUS_PATH
        .as_ref()?
        .captures(path)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

/// Canonical identity for a submitted article URL. Falls back to the trimmed
/// input when the string does not parse as a URL.
pub fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(mut url) = Url::parse(trimmed) else {
        return trimmed.to_owned();
    };

    // Like the WHATWG setters, an impossible change is silently ignored.
    let _ignored = url.set_scheme("https");
    if let Some(host) = url.host_str() {
        let lowered = host.to_lowercase();
        let without_www = lowered.strip_prefix("www.").unwrap_or(&lowered);
        let canonical = without_www
            .strip_suffix('.')
            .unwrap_or(without_www)
            .to_owned();
        if canonical != host {
            let _ignored = url.set_host(Some(&canonical));
        }
    }
    url.set_fragment(None);

    if let (Some(host), Some(id)) = (url.host_str(), x_status_id(url.path()))
        && X_HOSTS.contains(&host)
    {
        return format!("https://x.com/i/status/{id}");
    }

    let mut kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| !is_tracking_param(key))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    // Stable: equal keys keep their source order.
    kept.sort_by(|(a, _), (b, _)| js_cmp(a, b));
    url.set_query(None);
    if !kept.is_empty() {
        url.query_pairs_mut().extend_pairs(kept);
    }

    let path = url.path().to_owned();
    if path.len() > 1 && path.ends_with('/') {
        url.set_path(path.trim_end_matches('/'));
    }

    url.to_string()
}

/// The article's site for grouping: the lowercase host without a leading
/// `www.`, or `None` when the string is not a URL with a host.
pub fn url_domain(raw: &str) -> Option<String> {
    Url::parse(&normalize_url(raw))
        .ok()?
        .host_str()
        .map(str::to_owned)
}

#[cfg(test)]
mod url_spec {
    //! URL normalization cases.
    use super::normalize_url;

    #[test]
    fn strips_utm_and_known_tracking_params() {
        assert_eq!(
            normalize_url("https://example.com/a?utm_source=x&utm_medium=ios&ref=abc"),
            "https://example.com/a"
        );
    }

    #[test]
    fn keeps_content_bearing_query_params() {
        assert_eq!(
            normalize_url("https://example.com/?p=123&utm_campaign=x"),
            "https://example.com/?p=123"
        );
    }

    #[test]
    fn drops_the_fragment() {
        assert_eq!(
            normalize_url("https://example.com/a#section-2"),
            "https://example.com/a"
        );
    }

    #[test]
    fn lowercases_scheme_and_host_and_strips_a_leading_www() {
        assert_eq!(
            normalize_url("HTTPS://WWW.Example.COM/Path"),
            "https://example.com/Path"
        );
    }

    #[test]
    fn removes_a_trailing_slash_but_keeps_the_root_slash() {
        assert_eq!(
            normalize_url("https://example.com/a/b/"),
            "https://example.com/a/b"
        );
        assert_eq!(
            normalize_url("https://example.com/"),
            "https://example.com/"
        );
    }

    #[test]
    fn orders_remaining_params_so_param_order_doesnt_change_identity() {
        assert_eq!(
            normalize_url("https://example.com/a?b=2&a=1"),
            normalize_url("https://example.com/a?a=1&b=2")
        );
    }

    #[test]
    fn collapses_two_tracking_only_variants_of_the_same_article_to_one_identity() {
        let a = normalize_url("https://www.natesilver.net/p/x?r=7esws&utm_medium=ios");
        let b =
            normalize_url("https://natesilver.net/p/x?utm_medium=ios&r=7esws&triedRedirect=true");
        assert_eq!(a, b);
    }

    #[test]
    fn collapses_http_and_https_variants_of_the_same_article() {
        assert_eq!(
            normalize_url("http://example.com/a"),
            normalize_url("https://example.com/a")
        );
    }

    #[test]
    fn strips_a_trailing_dns_root_dot_from_the_host() {
        assert_eq!(
            normalize_url("https://example.com./a"),
            normalize_url("https://example.com/a")
        );
    }

    #[test]
    fn keeps_genuinely_different_articles_distinct() {
        assert_ne!(
            normalize_url("https://example.com/a"),
            normalize_url("https://example.com/b")
        );
    }

    #[test]
    fn canonicalizes_x_share_links_by_status_id() {
        assert_eq!(
            normalize_url(
                "https://x.com/edels0n/status/2077031491045929255?s=46&t=LN32clxPq8AlS6Ujqu_UEg"
            ),
            "https://x.com/i/status/2077031491045929255"
        );
    }

    #[test]
    fn collapses_x_and_twitter_host_username_and_media_path_variants() {
        let canonical = "https://x.com/i/status/2077031491045929255";
        assert_eq!(
            normalize_url(
                "https://mobile.twitter.com/old_handle/status/2077031491045929255/photo/1"
            ),
            canonical
        );
        assert_eq!(
            normalize_url("https://www.x.com/new_handle/status/2077031491045929255"),
            canonical
        );
        assert_eq!(
            normalize_url("https://x.com/i/web/status/2077031491045929255?utm_source=share"),
            canonical
        );
    }

    #[test]
    fn keeps_short_query_parameters_on_non_x_urls() {
        assert_eq!(
            normalize_url("https://example.com/article?s=46&t=chapter-2"),
            "https://example.com/article?s=46&t=chapter-2"
        );
    }

    #[test]
    fn returns_the_trimmed_input_when_the_string_is_not_a_url() {
        assert_eq!(normalize_url("  not a url  "), "not a url");
    }

    #[test]
    fn upgrades_an_explicit_default_https_port_like_whatwg() {
        assert_eq!(
            normalize_url("http://example.com:443/a"),
            "https://example.com/a"
        );
    }
}
