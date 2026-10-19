//! Event feeds: Omni's subscriptions to integration events, one per event
//! and argument set, with their polling state and replay gaps.

use std::collections::HashMap;

use leptos::prelude::*;
use omni_api::gateway::EventFeedView;
use omni_web_kit::components::{EmptyState, Panel, Status, StatusKind};
use omni_web_kit::utils::format::{format_absolute, format_relative_at};
use omni_web_kit::utils::js::locale_number;

use super::Gateway;
use super::logic;

#[component]
pub(super) fn FeedsPanel(gw: Gateway) -> impl IntoView {
    let feeds = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| o.event_feeds.clone())
                .unwrap_or_default()
        })
    });
    let names = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| {
                    o.integrations
                        .iter()
                        .map(|i| (i.id.clone(), i.name.clone()))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default()
        })
    });
    let row = move |feed: EventFeedView| {
        let (kind, word) = logic::feed_status(feed.state);
        let integration = names
            .with_untracked(|n| n.get(&feed.integration).cloned())
            .unwrap_or_else(|| feed.integration.clone());
        let arguments = feed
            .arguments
            .iter()
            .map(|(k, v)| view! { <span class="tag">{format!("{k}={v}")}</span> })
            .collect_view();
        let no_arguments = feed.arguments.is_empty();
        let last_poll = feed.last_poll_at.map(|at| at as f64);
        let next_poll = feed.next_poll_at.map(|at| at as f64);
        let last_gap = feed.last_gap_at.map(|at| at as f64);
        let gaps = feed.gaps;
        let gaps_class = if gaps > 0 {
            "numeric nowrap hide-below-desk text-warn"
        } else {
            "numeric nowrap hide-below-desk muted"
        };
        view! {
            <tr>
                <td class="cell-status">
                    <Status kind label=word dot_only=kind == StatusKind::Ok/>
                </td>
                <td class="grow">
                    <div class="cell-two">
                        <span class="truncate mono strong" title=feed.event.clone()>{feed.event.clone()}</span>
                        <span class="truncate small muted">{format!("{integration} · {}", feed.upstream_event)}</span>
                        {feed.error.as_ref().map(|e| view! {
                            <span class="small text-fault gw-feed-error" title=e.hint.clone()>
                                {format!("{}: {}", logic::error_kind_label(e.kind), e.message)}
                            </span>
                        })}
                    </div>
                </td>
                <td class="hide-below-wide">
                    {if no_arguments {
                        view! { <span class="muted">"—"</span> }.into_any()
                    } else {
                        view! { <div class="chips">{arguments}</div> }.into_any()
                    }}
                </td>
                <td class="numeric hide-below-desk">{feed.subscribers}</td>
                <td class="numeric nowrap hide-below-desk" title=last_poll.map(format_absolute)>
                    {move || last_poll.map_or_else(|| "—".to_owned(), |at| logic::ago(at, gw.now.get()))}
                </td>
                <td class="numeric nowrap hide-below-wide" title=next_poll.map(format_absolute)>
                    {move || next_poll.map_or_else(|| "—".to_owned(), |at| format_relative_at(at, gw.now.get()))}
                </td>
                <td class="numeric">{locale_number(feed.published as f64)}</td>
                <td
                    class=gaps_class
                    title=last_gap.map(|at| format!("Last gap {}", format_absolute(at)))
                >
                    {gaps}
                    {move || last_gap.filter(|_| gaps > 0).map(|at| view! {
                        <span class="small">{format!(" · {}", logic::ago(at, gw.now.get()))}</span>
                    })}
                </td>
            </tr>
        }
    };
    view! {
        <Panel
            title="Event feeds"
            class="gw-feeds"
            head_end=ViewFn::from(move || view! {
                <span class="hide-phone">"Polled for subscribed clients"</span>
                <span class="num">{move || feeds.with(Vec::len)}</span>
            })
        >
            {move || if feeds.with(Vec::is_empty) {
                view! {
                    <EmptyState
                        compact=true
                        message="No event feeds. A feed starts when a client subscribes to an integration's event."
                    />
                }
                .into_any()
            } else {
                view! {
                    <div class="table-wrap">
                        <table class="table dense gw-feed-table">
                            <thead>
                                <tr>
                                    <th class="cell-status"><span class="sr-only">"State"</span></th>
                                    <th>"Event"</th>
                                    <th class="hide-below-wide">"Arguments"</th>
                                    <th class="numeric hide-below-desk">"Subscribers"</th>
                                    <th class="numeric hide-below-desk">"Last poll"</th>
                                    <th class="numeric hide-below-wide">"Next poll"</th>
                                    <th class="numeric">"Published"</th>
                                    <th class="numeric hide-below-desk">"Gaps"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For each=move || feeds.get() key=|feed| format!("{feed:?}") children=row/>
                            </tbody>
                        </table>
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}
