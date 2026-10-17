//! The 232 px desktop rail (64 px collapsed with `[`): brand, search, the
//! grouped destinations with badges, the On air streamer rows and the
//! connection footer.

use leptos::prelude::*;
use omni_api::streamers::StreamerView;
use omni_web_kit::components::streamers::{
    platform_label, preferred_watch, streamer_path, viewer_number,
};
use omni_web_kit::components::{Glyph, Icon, IconSize, TickNum};
use omni_web_kit::live::use_live_data;
use omni_web_kit::router::Link;
use omni_web_kit::utils::js::locale_number;
use omni_web_kit::utils::tasks::task_health;

use super::ShellContext;
use super::connection::Connection;
use super::nav::{Group, LIVE_HREF, NAV, NavItem};

const GROUPS: [Group; 4] = [Group::Watch, Group::Listen, Group::Personal, Group::System];

const MAX_RAIL_STREAMS: usize = 3;

/// `path` is the streamer page `href` or one of its subpages.
fn on_streamer(path: &str, href: &str) -> bool {
    path == href
        || path
            .strip_prefix(href)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[component]
fn RailItem(
    item: NavItem,
    current: Signal<Option<&'static str>>,
    /// Failure count badge.
    #[prop(into)]
    badge: Signal<Option<usize>>,
) -> impl IntoView {
    let is_current = move || current.get() == Some(item.href);
    view! {
        <Link
            to=item.href
            class="rail-item"
            title=item.label
            aria_current=Signal::derive(move || is_current().then(|| "page".to_owned()))
        >
            <Glyph icon=item.icon/>
            <span class="label">{item.label}</span>
            {item.key.map(|k| view! { <span class="kbd" aria-hidden="true">{format!("g {k}")}</span> })}
            {move || badge.get().map(|n| view! { <span class="count fault">{n}</span> })}
        </Link>
    }
}

#[component]
pub fn Rail(
    shell: ShellContext,
    #[prop(into)] path: Signal<String>,
    #[prop(into)] current: Signal<Option<&'static str>>,
) -> impl IntoView {
    let live = use_live_data();
    let now = omni_web_kit::hooks::use_now(30_000);

    let live_ids = Memo::new(move |_| {
        live.snapshot.with(|s| {
            s.as_ref()
                .map(|s| {
                    s.streamers
                        .iter()
                        .filter(|v| v.is_live())
                        .map(|v| v.id().to_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    // Keep the System group within reach on short windows: at most three
    // streamer rows, the last becoming a link to the rest.
    let shown_live_ids = Memo::new(move |_| {
        live_ids.with(|ids| {
            let shown = if ids.len() > MAX_RAIL_STREAMS {
                MAX_RAIL_STREAMS - 1
            } else {
                ids.len()
            };
            ids[..shown].to_vec()
        })
    });
    let failing = Memo::new(move |_| {
        let now = now.get();
        live.snapshot.with(|s| {
            s.as_ref().map_or(0, |s| {
                s.tasks
                    .iter()
                    .filter(|t| task_health(t, now).needs_attention())
                    .count()
            })
        })
    });

    let badge_for = move |href: &'static str| -> Signal<Option<usize>> {
        Signal::derive(move || match href {
            "/operations" => Some(failing.get()).filter(|n| *n > 0),
            _ => None,
        })
    };

    let stream_row = move |id: String| {
        let streamer = Memo::new(move |_| {
            live.snapshot.with(|s| {
                s.as_ref().and_then(|s| {
                    s.streamers.iter().find_map(|v| match v {
                        StreamerView::Live(l) if l.id == id => Some(l.clone()),
                        _ => None,
                    })
                })
            })
        });
        let href = streamer.with_untracked(|s| s.as_ref().map(|s| streamer_path(&s.id)));
        let name = Signal::derive(move || {
            streamer.with(|s| {
                s.as_ref()
                    .map(|s| s.display_name.clone())
                    .unwrap_or_default()
            })
        });
        let watch = Memo::new(move |_| streamer.with(|s| s.as_ref().map(preferred_watch)));
        let platform =
            move || watch.with(|w| w.as_ref().map(|w| w.platform.clone()).unwrap_or_default());
        let watch_label = Signal::derive(move || {
            let name = name.get();
            watch.with(|w| match w {
                Some(w) => format!("Watch {name} on {}", platform_label(&w.platform)),
                None => format!("Watch {name}"),
            })
        });
        let viewers = Signal::derive(move || {
            streamer.with(|s| {
                s.as_ref()
                    .map(|s| locale_number(viewer_number(s) as f64))
                    .unwrap_or_default()
            })
        });
        let to = href.clone().unwrap_or_default();
        let current_path = to.clone();
        view! {
            <div class="rail-stream">
                <a
                    class="rail-item rail-watch"
                    href=move || watch.with(|w| w.as_ref().map(|w| w.url.clone()).unwrap_or_default())
                    target="_blank"
                    rel="noopener"
                    aria-label=watch_label
                    title=watch_label
                >
                    <span class=move || format!("pf-tick {}", platform()) aria-hidden="true"></span>
                    <span class="label">{move || name.get()}</span>
                    <TickNum value=viewers class="small dim"/>
                    <Glyph icon=Icon::External size=IconSize::Small class="watch-hint"/>
                </a>
                <Link
                    to=to
                    class="rail-item rail-details"
                    aria_label=Signal::derive(move || format!("{} details", name.get()))
                    title=Signal::derive(move || format!("{} details", name.get()))
                    aria_current=Signal::derive(move || {
                        path.with(|p| on_streamer(p, &current_path)).then(|| "page".to_owned())
                    })
                >
                    <Glyph icon=Icon::ChevronRight size=IconSize::Small/>
                </Link>
            </div>
        }
    };

    let collapsed = shell.rail_collapsed;
    view! {
        <nav class="rail" aria-label="Main">
            <div class="rail-brand">
                <img class="brand-mark" src="/icons/mark.svg" alt="" aria-hidden="true"/>
                <span class="label">"Omni"</span>
                <button
                    type="button"
                    class="btn ghost sm icon-only rail-toggle"
                    aria-label=move || if collapsed.get() { "Expand sidebar" } else { "Collapse sidebar" }
                    title="Toggle sidebar  ["
                    aria-pressed=move || collapsed.get().to_string()
                    on:click=move |_| shell.toggle_rail()
                >
                    <Glyph icon=Icon::Sidebar size=IconSize::Small/>
                </button>
            </div>
            <button type="button" class="rail-search" on:click=move |_| shell.palette_open.set(true) aria-label="Search">
                <Glyph icon=Icon::Search size=IconSize::Small/>
                <span>"Search"</span>
                <kbd class="kbd">"⌘K"</kbd>
            </button>
            <div class="rail-nav">
                <RailItem item=NAV[0] current badge=Signal::stored(None)/>
                <div class="rail-group">
                    <Link
                        to=LIVE_HREF
                        class="rail-item"
                        title="On air"
                        aria_current=Signal::derive(move || {
                            (current.get() == Some(LIVE_HREF)
                                && !live_ids.with(|ids| ids.iter().any(|id| path.with(|p| on_streamer(p, &streamer_path(id))))))
                                .then(|| "page".to_owned())
                        })
                    >
                        <Glyph icon=Icon::Live/>
                        <span class="label">"On air"</span>
                        <span class="kbd" aria-hidden="true">"g l"</span>
                        {move || {
                            let n = live_ids.with(Vec::len);
                            (n > 0).then(|| view! { <span class="count live">{n}</span> })
                        }}
                        {move || live_ids.with(|ids| !ids.is_empty()).then(|| view! { <span class="rail-dot" aria-hidden="true"></span> })}
                    </Link>
                    <For each=move || shown_live_ids.get() key=|id| id.clone() children=stream_row/>
                    {move || {
                        let hidden = live_ids.with(Vec::len).saturating_sub(shown_live_ids.with(Vec::len));
                        (hidden > 0).then(|| view! {
                            <Link to=LIVE_HREF class="rail-item rail-more">{format!("+{hidden} more on air")}</Link>
                        })
                    }}
                    {move || {
                        (live.snapshot.with(Option::is_some) && live_ids.with(Vec::is_empty))
                            .then(|| view! { <span class="rail-empty">"Nobody live"</span> })
                    }}
                </div>
                {GROUPS
                    .into_iter()
                    .map(|group| {
                        view! {
                            <div class="rail-group" role="group" aria-label=group.label()>
                                <span class="rail-label">{group.label()}</span>
                                {NAV
                                    .iter()
                                    .filter(|i| i.group == group)
                                    .map(|item| view! { <RailItem item=*item current badge=badge_for(item.href)/> })
                                    .collect_view()}
                            </div>
                        }
                    })
                    .collect_view()}
            </div>
            <div class="rail-foot">
                <Connection/>
            </div>
        </nav>
    }
}
