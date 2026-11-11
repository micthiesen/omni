//! Minimal CalDAV multistatus parsing.
//!
//! Regex-based on purpose: iCloud emits simple, flat PROPFIND
//! responses whose namespace prefixes vary (`d:`, `D:`, none, `A:`, ...), so
//! matching is prefix-agnostic.

use std::sync::LazyLock;

use regex::Regex;

/// `(?:[\w-]+:)?` with JS's ASCII `\w`.
const PREFIX: &str = "(?:[A-Za-z0-9_-]+:)?";

fn compile(pattern: &str) -> Option<Regex> {
    Regex::new(pattern).ok()
}

static HREF: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(&format!("(?i)<{PREFIX}href[^>]*>([^<]+)</{PREFIX}href>")));
static RESPONSE_SPLIT: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(&format!(r"(?i)<{PREFIX}response[>\s]")));
static RESOURCETYPE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(&format!(
        r"(?is)<{PREFIX}resourcetype[^>]*>(.*?)</{PREFIX}resourcetype>"
    ))
});
/// `<calendar/>`, `<c:calendar/>` and iCloud's attribute-carrying
/// `<calendar xmlns="..."/>`, never `<calendar-proxy-*>`.
static CALENDAR_ELEMENT: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(&format!(r"(?i)<{PREFIX}calendar(?:>|[\s/][^>]*>)")));
static DISPLAYNAME: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(&format!(
        r"(?i)<{PREFIX}displayname[^>]*>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</{PREFIX}displayname>"
    ))
});
static COMPONENT_SET: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(&format!(
        r"(?is)<{PREFIX}supported-calendar-component-set[^>]*>(.*?)</{PREFIX}supported-calendar-component-set>"
    ))
});
static COMPONENT_NAME: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r#"(?i)name=["']([A-Z]+)["']"#));

/// The href inside a named property element, e.g.
/// `<current-user-principal><href>/123/principal/</href></current-user-principal>`.
pub fn extract_property_href(xml: &str, property: &str) -> Option<String> {
    let property = regex::escape(property);
    let block = compile(&format!(
        r"(?is)<{PREFIX}{property}[^>]*>(.*?)</{PREFIX}{property}>"
    ))?;
    let inner = block.captures(xml)?.get(1)?.as_str();
    let href = HREF.as_ref()?.captures(inner)?.get(1)?.as_str().trim();
    (!href.is_empty()).then(|| href.to_owned())
}

/// One calendar collection from a Depth:1 PROPFIND.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CalendarCollection {
    pub href: String,
    pub name: String,
    /// VEVENT/VTODO/... when the server reported a component set.
    pub components: Option<Vec<String>>,
}

/// Calendar collections in a Depth:1 PROPFIND multistatus response. A response
/// counts as a calendar when its resourcetype contains a bare `<calendar/>`
/// element.
pub fn extract_calendar_collections(xml: &str) -> Vec<CalendarCollection> {
    let (Some(split), Some(resourcetype), Some(calendar), Some(href), Some(displayname)) = (
        RESPONSE_SPLIT.as_ref(),
        RESOURCETYPE.as_ref(),
        CALENDAR_ELEMENT.as_ref(),
        HREF.as_ref(),
        DISPLAYNAME.as_ref(),
    ) else {
        return Vec::new();
    };
    let mut results = Vec::new();
    for block in split.split(xml).skip(1) {
        let is_calendar = resourcetype
            .captures(block)
            .and_then(|c| c.get(1))
            .is_some_and(|inner| calendar.is_match(inner.as_str()));
        if !is_calendar {
            continue;
        }
        let Some(href_match) = href.captures(block).and_then(|c| c.get(1)) else {
            continue;
        };
        let name = displayname
            .captures(block)
            .and_then(|c| c.get(1))
            .map_or_else(|| "Unnamed".to_owned(), |m| m.as_str().to_owned());
        let components = COMPONENT_SET
            .as_ref()
            .and_then(|re| re.captures(block))
            .and_then(|c| c.get(1))
            .map(|inner| {
                COMPONENT_NAME
                    .as_ref()
                    .map(|re| {
                        re.captures_iter(inner.as_str())
                            .filter_map(|c| c.get(1))
                            .map(|m| m.as_str().to_uppercase())
                            .collect()
                    })
                    .unwrap_or_default()
            });
        results.push(CalendarCollection {
            href: href_match.as_str().trim().to_owned(),
            name,
            components,
        });
    }
    results
}

/// The collection to write events to: the configured name first
/// (case-insensitive), then common defaults, then the first VEVENT-capable
/// collection. Collections reporting a component set without VEVENT (task
/// lists, reminders) are never picked.
pub fn pick_calendar_collection<'a>(
    collections: &'a [CalendarCollection],
    preferred_name: Option<&str>,
) -> Option<&'a CalendarCollection> {
    let event_capable: Vec<&CalendarCollection> = collections
        .iter()
        .filter(|c| {
            c.components
                .as_ref()
                .is_none_or(|components| components.iter().any(|c| c == "VEVENT"))
        })
        .collect();
    if let Some(preferred) = preferred_name.filter(|name| !name.is_empty()) {
        let preferred = preferred.to_lowercase();
        if let Some(found) = event_capable
            .iter()
            .find(|c| c.name.to_lowercase() == preferred)
        {
            return Some(found);
        }
    }
    // In preference order; "iCloud" is the account-default calendar's name.
    for fallback in ["icloud", "default", "personal", "home"] {
        if let Some(found) = event_capable
            .iter()
            .find(|c| c.name.to_lowercase() == fallback)
        {
            return Some(found);
        }
    }
    event_capable.first().copied()
}

/// The resource href of every `response` in a multistatus whose block
/// contains `needle` (a REPORT's matches, narrowed by their returned data).
pub fn extract_response_hrefs_containing(xml: &str, needle: &str) -> Vec<String> {
    let (Some(split), Some(href)) = (RESPONSE_SPLIT.as_ref(), HREF.as_ref()) else {
        return Vec::new();
    };
    split
        .split(xml)
        .skip(1)
        .filter(|block| block.contains(needle))
        .filter_map(|block| href.captures(block).and_then(|c| c.get(1)))
        .map(|m| m.as_str().trim().to_owned())
        .collect()
}
