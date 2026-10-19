//! The information architecture: rail groups, `g` shortcuts, the current
//! item of a path and its breadcrumbs. Pure, so it is unit-tested natively.

use omni_web_kit::components::Icon;

/// A rail group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Top,
    Watch,
    Listen,
    Personal,
    System,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Top => "",
            Group::Watch => "Watch",
            Group::Listen => "Listen",
            Group::Personal => "Personal",
            Group::System => "System",
        }
    }

    /// Where the group's crumb links (its first item).
    pub fn href(self) -> &'static str {
        match self {
            Group::Top => "/",
            Group::Watch => "/media",
            Group::Listen => "/podcasts",
            Group::Personal => "/emails",
            Group::System => "/operations",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavItem {
    pub label: &'static str,
    pub href: &'static str,
    pub icon: Icon,
    pub group: Group,
    /// Second key of the `g` shortcut.
    pub key: Option<char>,
}

const fn item(
    label: &'static str,
    href: &'static str,
    icon: Icon,
    group: Group,
    key: Option<char>,
) -> NavItem {
    NavItem {
        label,
        href,
        icon,
        group,
        key,
    }
}

/// Every destination except `/live` (the On air header), in rail order.
pub const NAV: [NavItem; 16] = [
    item("Home", "/", Icon::Home, Group::Top, Some('h')),
    item("Movies & TV", "/media", Icon::Film, Group::Watch, Some('m')),
    item(
        "Podcasts",
        "/podcasts",
        Icon::Headphones,
        Group::Listen,
        Some('p'),
    ),
    item("PressPods", "/pods", Icon::Mic, Group::Listen, None),
    item("Email", "/emails", Icon::Mail, Group::Personal, Some('e')),
    item(
        "Reminders",
        "/reminders",
        Icon::CheckSquare,
        Group::Personal,
        None,
    ),
    item(
        "Calendar",
        "/calendar",
        Icon::Calendar,
        Group::Personal,
        Some('a'),
    ),
    item(
        "Deliveries",
        "/deliveries",
        Icon::Package,
        Group::Personal,
        Some('v'),
    ),
    item("Pets", "/pets", Icon::Paw, Group::Personal, None),
    item("Stylist", "/stylist", Icon::Hanger, Group::Personal, None),
    item(
        "Operations",
        "/operations",
        Icon::Pulse,
        Group::System,
        Some('o'),
    ),
    item("Costs", "/costs", Icon::Coin, Group::System, Some('c')),
    item("MCP", "/mcp-activity", Icon::Plug, Group::System, None),
    item("MCP gateway", "/gateway", Icon::Grid, Group::System, None),
    item("Claude", "/claude", Icon::Terminal, Group::System, None),
    item("Data", "/data", Icon::Database, Group::System, Some('d')),
];

pub const LIVE_HREF: &str = "/live";
/// Tracked-streamer management, under Live.
pub const STREAMER_CONFIG_HREF: &str = "/live/streamers";

/// The `g` shortcut target for `key`.
pub fn shortcut_target(key: char) -> Option<&'static str> {
    if key == 'l' {
        return Some(LIVE_HREF);
    }
    NAV.iter().find(|i| i.key == Some(key)).map(|i| i.href)
}

/// The rail href that is current for `path` (`/live` covers streamer pages
/// whose streamer is offline; the shell marks live ones on their own row).
pub fn current_href(path: &str) -> Option<&'static str> {
    if path == LIVE_HREF || path == STREAMER_CONFIG_HREF || path.starts_with("/streamers/") {
        return Some(LIVE_HREF);
    }
    if path.starts_with("/feedback/recommendations/") {
        return Some("/media");
    }
    if path.starts_with("/feedback/podcasts/") {
        return Some("/podcasts");
    }
    NAV.iter()
        .filter(|i| i.href != "/")
        .find(|i| path == i.href || path.starts_with(&format!("{}/", i.href)))
        .map(|i| i.href)
        .or((path == "/").then_some("/"))
}

pub fn item_for(href: &str) -> Option<&'static NavItem> {
    NAV.iter().find(|i| i.href == href)
}

/// One breadcrumb; `href` is `None` for the current page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crumb {
    pub label: String,
    pub href: Option<String>,
}

fn link(label: impl Into<String>, href: impl Into<String>) -> Crumb {
    Crumb {
        label: label.into(),
        href: Some(href.into()),
    }
}

fn here(label: impl Into<String>) -> Crumb {
    Crumb {
        label: label.into(),
        href: None,
    }
}

/// Names the shell resolves for crumbs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CrumbNames {
    /// The page's own label (streamer, pick title).
    pub page: Option<String>,
    /// The streamer's display name (`/streamers/:id`).
    pub streamer: Option<String>,
}

/// Breadcrumbs for a normalized path. `streamer_id` comes from the route;
/// `names` fills dynamic labels.
pub fn crumbs(path: &str, streamer_id: Option<&str>, names: &CrumbNames) -> Vec<Crumb> {
    let page = names.page.clone();
    if path == "/" {
        return vec![here("Home")];
    }
    if path == LIVE_HREF {
        return vec![here("Live")];
    }
    if path == STREAMER_CONFIG_HREF {
        return vec![link("Live", LIVE_HREF), here("Streamers")];
    }
    if let Some(rest) = path.strip_prefix("/streamers/") {
        let id = streamer_id.unwrap_or(rest);
        let name = names
            .streamer
            .clone()
            .or_else(|| page.clone())
            .unwrap_or_else(|| id.to_owned());
        return if path.ends_with("/intelligence") {
            vec![
                link("Live", LIVE_HREF),
                link(
                    name,
                    format!("/streamers/{}", omni_api::common::encode_uri_component(id)),
                ),
                here("Intelligence"),
            ]
        } else {
            vec![link("Live", LIVE_HREF), here(name)]
        };
    }
    if let Some(current) = current_href(path)
        && let Some(item) = item_for(current)
    {
        if path == item.href {
            return vec![
                link(item.group.label(), item.group.href()),
                here(item.label),
            ];
        }
        let detail = if path.starts_with("/feedback/") {
            "Feedback".to_owned()
        } else {
            page.unwrap_or_else(|| "Details".to_owned())
        };
        return vec![link(item.label, item.href), here(detail)];
    }
    vec![here("Not found")]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(crumbs: &[Crumb]) -> Vec<(&str, bool)> {
        crumbs
            .iter()
            .map(|c| (c.label.as_str(), c.href.is_some()))
            .collect()
    }

    #[test]
    fn current_item_follows_sections() {
        assert_eq!(current_href("/"), Some("/"));
        assert_eq!(current_href("/media/abc"), Some("/media"));
        assert_eq!(current_href("/feedback/podcasts/x"), Some("/podcasts"));
        assert_eq!(current_href("/workspaces/w/s"), None);
        assert_eq!(current_href("/briefings"), None);
        assert_eq!(current_href("/streamers/hutch"), Some("/live"));
        assert_eq!(current_href("/live/streamers"), Some("/live"));
        assert_eq!(current_href("/deliveries"), Some("/deliveries"));
        assert_eq!(current_href("/calendar"), Some("/calendar"));
        assert_eq!(current_href("/stylist"), Some("/stylist"));
        assert_eq!(current_href("/stylistx"), None);
        assert_eq!(current_href("/gateway"), Some("/gateway"));
        assert_eq!(current_href("/podsx"), None);
        assert_eq!(current_href("/nope"), None);
    }

    #[test]
    fn shortcuts_cover_the_documented_keys() {
        for (key, href) in [
            ('h', "/"),
            ('l', "/live"),
            ('m', "/media"),
            ('p', "/podcasts"),
            ('e', "/emails"),
            ('o', "/operations"),
            ('c', "/costs"),
            ('d', "/data"),
            ('a', "/calendar"),
            ('v', "/deliveries"),
        ] {
            assert_eq!(shortcut_target(key), Some(href), "g {key}");
        }
        assert_eq!(shortcut_target('z'), None);
        assert_eq!(shortcut_target('w'), None);
        assert_eq!(shortcut_target('b'), None);
    }

    #[test]
    fn crumbs_name_the_trail() {
        let names = CrumbNames {
            streamer: Some("Hutch".into()),
            ..CrumbNames::default()
        };
        assert_eq!(
            labels(&crumbs(
                "/streamers/hutch/intelligence",
                Some("hutch"),
                &names
            )),
            [("Live", true), ("Hutch", true), ("Intelligence", false)]
        );
        assert_eq!(
            labels(&crumbs("/live/streamers", None, &CrumbNames::default())),
            [("Live", true), ("Streamers", false)]
        );
        assert_eq!(
            labels(&crumbs("/operations", None, &CrumbNames::default())),
            [("System", true), ("Operations", false)]
        );
        assert_eq!(
            labels(&crumbs("/stylist", None, &CrumbNames::default())),
            [("Personal", true), ("Stylist", false)]
        );
        assert_eq!(
            labels(&crumbs("/gateway", None, &CrumbNames::default())),
            [("System", true), ("MCP gateway", false)]
        );
        let names = CrumbNames {
            page: Some("Dune".into()),
            ..CrumbNames::default()
        };
        assert_eq!(
            labels(&crumbs("/media/rec1", None, &names)),
            [("Movies & TV", true), ("Dune", false)]
        );
        assert_eq!(
            labels(&crumbs("/nope", None, &CrumbNames::default())),
            [("Not found", false)]
        );
    }
}
