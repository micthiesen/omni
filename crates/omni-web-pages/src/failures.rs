//! `/failures`: what is failing across Omni in one list, so Operations, MCP
//! activity and the gateway need not be checked one by one. Polls
//! `GET /api/failures` every 30 s while the tab is visible.

use std::time::Duration;

use leptos::prelude::*;
use omni_api::failures::{FailureCounts, FailureItem, FailureKind, FailuresResponse};
use omni_web_kit::api::{self, ApiClientError};
use omni_web_kit::components::{
    Button, ButtonSize, EmptyState, ErrorState, Glyph, Icon, IconSize, InlineNote, PageHead, Panel,
    Readout, ReadoutBand, SegOption, Segmented, ShowMoreButton, SkeletonRows, Tag, Tone,
    use_show_more,
};
use omni_web_kit::hooks::{use_now, use_visible_poll};
use omni_web_kit::router::Link;
use omni_web_kit::utils::format::{format_absolute, format_relative_at};
use omni_web_kit::utils::js::locale_number;

const POLL: Duration = Duration::from_secs(30);
const PAGE: usize = 50;
const KINDS: [FailureKind; 4] = [
    FailureKind::Task,
    FailureKind::ToolCall,
    FailureKind::Delivery,
    FailureKind::Integration,
];

/// The row's kind Tag.
fn kind_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Task => "Task",
        FailureKind::ToolCall => "Tool call",
        FailureKind::Delivery => "Delivery",
        FailureKind::Integration => "Integration",
    }
}

/// The filter segment (`None` is All).
fn filter_label(filter: Option<FailureKind>) -> &'static str {
    match filter {
        None => "All",
        Some(FailureKind::Task) => "Tasks",
        Some(FailureKind::ToolCall) => "Tool calls",
        Some(FailureKind::Delivery) => "Deliveries",
        Some(FailureKind::Integration) => "Integrations",
    }
}

fn readout_label(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Task => "Tasks",
        FailureKind::ToolCall => "Tool calls · 7d",
        FailureKind::Delivery => "Deliveries · 7d",
        FailureKind::Integration => "Integrations",
    }
}

fn readout_sub(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Task => "Last run failed or degraded",
        FailureKind::ToolCall => "Errors, invalid and interrupted",
        FailureKind::Delivery => "Event deliveries that gave up",
        FailureKind::Integration => "In error now",
    }
}

/// The filtered list's empty sentence.
fn none_of(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Task => "No task's last run failed.",
        FailureKind::ToolCall => "No tool call failed in the last 7 days.",
        FailureKind::Delivery => "No event delivery failed in the last 7 days.",
        FailureKind::Integration => "No integration is in error.",
    }
}

/// Tool names and event names are identifiers; task and integration titles
/// are display names.
fn mono_title(kind: FailureKind) -> bool {
    matches!(kind, FailureKind::ToolCall | FailureKind::Delivery)
}

fn count_of(counts: &FailureCounts, kind: FailureKind) -> u32 {
    match kind {
        FailureKind::Task => counts.tasks,
        FailureKind::ToolCall => counts.tool_calls,
        FailureKind::Delivery => counts.deliveries,
        FailureKind::Integration => counts.integrations,
    }
}

/// How many failures `filter` covers, counted before the server's 200-item
/// cap.
fn expected(counts: &FailureCounts, filter: Option<FailureKind>) -> u32 {
    match filter {
        Some(kind) => count_of(counts, kind),
        None => KINDS.iter().fold(0u32, |sum, kind| {
            sum.saturating_add(count_of(counts, *kind))
        }),
    }
}

fn filtered(items: &[FailureItem], filter: Option<FailureKind>) -> Vec<FailureItem> {
    items
        .iter()
        .filter(|item| filter.is_none_or(|kind| item.kind == kind))
        .cloned()
        .collect()
}

fn filter_options(counts: &FailureCounts) -> Vec<SegOption<Option<FailureKind>>> {
    std::iter::once(None)
        .chain(KINDS.map(Some))
        .map(|filter| {
            SegOption::new(filter, filter_label(filter))
                .with_count(expected(counts, filter) as usize)
        })
        .collect()
}

fn lede(counts: &FailureCounts) -> String {
    const SCOPE: &str = "Covers every task's last run and each integration as they stand now, \
                         plus tool calls and event deliveries from the last 7 days.";
    match expected(counts, None) {
        0 => SCOPE.to_owned(),
        1 => format!("1 thing is failing. {SCOPE}"),
        n => format!("{n} things are failing. {SCOPE}"),
    }
}

/// The server sends at most 200 items but counts every failure.
fn truncation(expected: u32, received: usize) -> Option<String> {
    (expected as usize > received).then(|| format!("Showing the newest {received} of {expected}."))
}

/// The page clock ticks every few seconds, so a failure newer than the last
/// tick reads "just now", never "in 7s".
fn ago(at: f64, now: f64) -> String {
    format_relative_at(at.min(now - 1.0), now)
}

#[component]
fn FailureRow(item: FailureItem, now: ReadSignal<f64>) -> impl IntoView {
    let FailureItem {
        kind,
        title,
        detail,
        at,
        link,
    } = item;
    let at = at as f64;
    let title_class = if mono_title(kind) {
        "row-title mono"
    } else {
        "row-title"
    };
    let title_hover = title.clone();
    let detail_hover = detail.clone();
    view! {
        <Link to=link class="row failures-row">
            <span class="failures-kind"><Tag>{kind_label(kind)}</Tag></span>
            <span class="row-main">
                <span class=title_class title=title_hover>{title}</span>
                <span class="row-sub failures-detail clamp-2" title=detail_hover>{detail}</span>
            </span>
            <span class="row-end">
                <span class="num small muted" title=format_absolute(at)>{move || ago(at, now.get())}</span>
                <Glyph icon=Icon::ChevronRight size=IconSize::Small/>
            </span>
        </Link>
    }
}

#[component]
pub fn FailuresPage() -> impl IntoView {
    let data = RwSignal::new(None::<FailuresResponse>);
    let error = RwSignal::new(None::<String>);
    let filter = RwSignal::new(None::<FailureKind>);
    let now = use_now(15_000);

    let reload = use_visible_poll(
        Signal::stored(String::new()),
        api::fetch_failures,
        move |next: FailuresResponse| {
            data.set(Some(next));
            if error.with_untracked(Option::is_some) {
                error.set(None);
            }
        },
        move |e: ApiClientError| error.set(Some(e.message().to_owned())),
        POLL,
    );

    // Memos, so a poll that only moves `checkedAt` rebuilds nothing.
    let items = Memo::new(move |_| data.with(|d| d.as_ref().map(|d| d.items.clone())));
    let counts = Memo::new(move |_| {
        data.with(|d| d.as_ref().map(|d| d.counts.clone()))
            .unwrap_or_default()
    });
    let loaded = Memo::new(move |_| items.with(Option::is_some));
    let all_clear = Memo::new(move |_| items.with(|i| i.as_ref().is_some_and(Vec::is_empty)));
    let shown = Memo::new(move |_| {
        let filter = filter.get();
        items.with(|i| {
            i.as_deref()
                .map(|i| filtered(i, filter))
                .unwrap_or_default()
        })
    });
    let shown_empty = Memo::new(move |_| shown.with(Vec::is_empty));
    let pages = use_show_more(
        shown.into(),
        PAGE,
        Signal::derive(move || filter_label(filter.get()).to_owned()),
    );
    let capped = Memo::new(move |_| {
        let expected = counts.with(|c| expected(c, filter.get()));
        truncation(expected, shown.with(Vec::len))
    });

    let lede_text = Signal::derive(move || loaded.get().then(|| counts.with(lede)));
    let options = Signal::derive(move || counts.with(filter_options));

    let readouts = move || {
        view! {
            <ReadoutBand cols=4 aria_label="Failures by kind">
                {KINDS
                    .into_iter()
                    .map(|kind| {
                        let n = Signal::derive(move || counts.with(|c| count_of(c, kind)));
                        view! {
                            <Readout
                                label=readout_label(kind)
                                value=Signal::derive(move || locale_number(f64::from(n.get())))
                                tone=Signal::derive(move || if n.get() > 0 { Tone::Fault } else { Tone::Neutral })
                            >
                                {readout_sub(kind)}
                            </Readout>
                        }
                    })
                    .collect_view()}
            </ReadoutBand>
        }
    };

    let list = move || {
        if all_clear.get() {
            return view! {
                <Panel>
                    <EmptyState
                        title="Nothing is failing"
                        icon=Icon::Check
                        message="Every task's last run succeeded, no integration is in error, and no tool call or event delivery failed in the last 7 days."
                    />
                </Panel>
            }
            .into_any();
        }
        view! {
            <Panel aria_label="Failures, newest first">
                <div class="toolbar failures-toolbar">
                    <Segmented
                        options
                        value=filter
                        on_change=Callback::new(move |f| filter.set(f))
                        aria_label="Failure kind"
                    />
                </div>
                {move || {
                    if shown_empty.get() {
                        let message = filter.get().map_or("Nothing is failing.", none_of);
                        return view! {
                            <EmptyState
                                compact=true
                                message
                                action=ViewFn::from(move || view! {
                                    <Button size=ButtonSize::Sm on_click=Callback::new(move |_| filter.set(None))>
                                        "Show all"
                                    </Button>
                                })
                            />
                        }
                        .into_any();
                    }
                    view! {
                        <div class="rows">
                            {move || pages
                                .visible
                                .get()
                                .into_iter()
                                .map(|item| view! { <FailureRow item now/> })
                                .collect_view()}
                        </div>
                    }
                    .into_any()
                }}
                {move || {
                    let more = pages.has_more.get();
                    let note = capped.get();
                    (more || note.is_some()).then(|| view! {
                        <div class="panel-foot failures-foot">
                            {note.map(|n| view! { <span class="small muted">{n}</span> })}
                            {more.then(|| view! {
                                <ShowMoreButton
                                    remaining=pages.remaining
                                    on_click=Callback::new(move |()| pages.show_more())
                                />
                            })}
                        </div>
                    })
                }}
            </Panel>
        }
        .into_any()
    };

    view! {
        <PageHead title="Failures" lede=lede_text/>
        {move || {
            if loaded.get() {
                return None;
            }
            Some(match error.get() {
                Some(e) => view! {
                    <ErrorState
                        title="Failures could not load"
                        detail="Operations, MCP activity and the MCP gateway still show their own."
                        raw=e
                        retry=reload
                        link=("Operations".to_owned(), "/operations".to_owned())
                        page=true
                    />
                }
                .into_any(),
                None => view! { <SkeletonRows count=6 label="Loading failures"/> }.into_any(),
            })
        }}
        {move || loaded.get().then(|| view! {
            <div class="stack-lg">
                {move || error.get().map(|e| view! {
                    <InlineNote tone=Tone::Warn role="status">
                        {format!("Refresh failed ({e}). Showing the last loaded state.")}
                    </InlineNote>
                })}
                {readouts}
                {list}
            </div>
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: FailureKind, at: i64) -> FailureItem {
        FailureItem {
            kind,
            title: format!("{kind:?}"),
            detail: "failed".to_owned(),
            at,
            link: "/operations".to_owned(),
        }
    }

    fn counts() -> FailureCounts {
        FailureCounts {
            tasks: 2,
            tool_calls: 250,
            deliveries: 0,
            integrations: 1,
        }
    }

    #[test]
    fn filter_keeps_order_and_kind() {
        let items = [
            item(FailureKind::ToolCall, 5),
            item(FailureKind::Task, 4),
            item(FailureKind::ToolCall, 3),
            item(FailureKind::Integration, 2),
        ];
        let calls = filtered(&items, Some(FailureKind::ToolCall));
        assert_eq!(
            calls.iter().map(|i| i.at).collect::<Vec<_>>(),
            [5, 3],
            "newest first, only tool calls"
        );
        assert_eq!(filtered(&items, None).len(), 4);
        assert!(filtered(&items, Some(FailureKind::Delivery)).is_empty());
    }

    #[test]
    fn options_count_every_kind() {
        let options = filter_options(&counts());
        let shape: Vec<(&str, Option<usize>)> = options
            .iter()
            .map(|o| (o.label.as_str(), o.count))
            .collect();
        assert_eq!(
            shape,
            [
                ("All", Some(253)),
                ("Tasks", Some(2)),
                ("Tool calls", Some(250)),
                ("Deliveries", Some(0)),
                ("Integrations", Some(1)),
            ]
        );
        assert_eq!(options[0].value, None);
        assert_eq!(options[2].value, Some(FailureKind::ToolCall));
    }

    #[test]
    fn lede_says_how_many_and_what_is_covered() {
        assert!(lede(&FailureCounts::default()).starts_with("Covers every task's last run"));
        assert!(lede(&counts()).starts_with("253 things are failing. Covers"));
        let one = FailureCounts {
            integrations: 1,
            ..FailureCounts::default()
        };
        assert!(lede(&one).starts_with("1 thing is failing. "));
    }

    #[test]
    fn truncation_names_the_server_cap() {
        assert_eq!(
            truncation(250, 197).as_deref(),
            Some("Showing the newest 197 of 250.")
        );
        assert_eq!(truncation(3, 3), None);
        assert_eq!(truncation(0, 0), None);
    }

    #[test]
    fn identifiers_are_mono() {
        assert!(mono_title(FailureKind::ToolCall));
        assert!(mono_title(FailureKind::Delivery));
        assert!(!mono_title(FailureKind::Task));
        assert!(!mono_title(FailureKind::Integration));
        assert_eq!(kind_label(FailureKind::ToolCall), "Tool call");
    }
}
