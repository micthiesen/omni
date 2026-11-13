//! Home: the status sentence, the On air Stage, Needs you, Up next, On deck
//! and the Inbox and Spend panels.

use leptos::prelude::*;
use omni_api::common::encode_uri_component;
use omni_api::costs::{CostRange, CostsResponse};
use omni_api::email::{EmailActivity, EmailActivityOutcome};
use omni_api::streamers::{OfflineStreamerView, StreamerTier, StreamerView};
use omni_api::tasks::TaskInfo;
use omni_web_kit::api;
use omni_web_kit::components::streamers::streamer_path;
use omni_web_kit::components::{
    Avatar, Disclosure, EmptyState, ErrorState, Glyph, Icon, IconSize, LiveTag, OnDeck, PageHead,
    Panel, Presence, Readout, ReadoutSize, Segmented, Skeleton, SkeletonKind, SkeletonRows, Status,
    StatusKind, Tone,
};
use omni_web_kit::feeds::use_task_backed;
use omni_web_kit::hooks::use_now;
use omni_web_kit::live::use_live_data;
use omni_web_kit::router::Link;
use omni_web_kit::task::spawn_scoped;
use omni_web_kit::utils::format::{format_cents, format_relative_at, task_label};
use omni_web_kit::utils::js::now_ms;
use omni_web_kit::utils::tasks::{
    Cadence, TaskHealth, cadence, format_next, next_run_ms, period_ms, task_health,
};
use omni_web_pages::{AgendaTile, use_calendar_status};

use super::live::{
    LeadStage, LiveRow, LiveSort, live_list, on_air_meta, sort_options, use_live_order,
};

const DAY_MS: f64 = 86_400_000.0;

/// One "Needs you" line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attention {
    pub count: usize,
    pub label: String,
    pub href: &'static str,
    pub kind: StatusKind,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Personal sources that can need you: a failing calendar sync, pets with a
/// tripped health rule.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PersonalAttention {
    pub calendar: bool,
    pub pets: usize,
}

/// Attention items from the snapshot tasks, failed emails in the last day
/// and the personal sources.
pub fn attention_items(
    tasks: &[TaskInfo],
    email_failures: usize,
    personal: PersonalAttention,
    now: f64,
) -> Vec<Attention> {
    let failing = tasks
        .iter()
        .filter(|t| task_health(t, now) == TaskHealth::Fault)
        .count();
    let stale = tasks
        .iter()
        .filter(|t| task_health(t, now) == TaskHealth::Stale)
        .count();
    let degraded = tasks
        .iter()
        .filter(|t| task_health(t, now) == TaskHealth::Degraded)
        .count();
    [
        (
            failing,
            "task failing",
            "tasks failing",
            "/operations?filter=attention",
            StatusKind::Fault,
        ),
        (
            degraded,
            "task degraded",
            "tasks degraded",
            "/operations?filter=attention",
            StatusKind::Warn,
        ),
        (
            stale,
            "task stale",
            "tasks stale",
            "/operations?filter=attention",
            StatusKind::Stale,
        ),
        (
            email_failures,
            "email failed today",
            "emails failed today",
            "/emails?outcome=failed",
            StatusKind::Fault,
        ),
        (
            usize::from(personal.calendar),
            "calendar sync failing",
            "calendar sync failing",
            "/calendar",
            StatusKind::Warn,
        ),
        (
            personal.pets,
            "pet health alert",
            "pet health alerts",
            "/pets",
            StatusKind::Warn,
        ),
    ]
    .into_iter()
    .filter(|(n, ..)| *n > 0)
    .map(|(count, one, many, href, kind)| Attention {
        count,
        label: plural(count, one, many),
        href,
        kind,
    })
    .collect()
}

/// The headline and the line under it.
pub fn status_sentence(items: &[Attention], live: usize, healthy: bool) -> (String, String) {
    let total: usize = items.iter().map(|i| i.count).sum();
    let channels = match live {
        0 => "nobody on air".to_owned(),
        n => format!("{} on air", plural(n, "channel", "channels")),
    };
    if total == 0 {
        let tasks = if healthy {
            "every task healthy"
        } else {
            "tasks loading"
        };
        (
            "All quiet.".to_owned(),
            format!(
                "{}, {tasks}, nothing waiting on you.",
                capitalize(&channels)
            ),
        )
    } else {
        let head = if total == 1 {
            "1 thing needs you.".to_owned()
        } else {
            format!("{total} things need you.")
        };
        let parts: Vec<String> = items.iter().map(|i| i.label.clone()).collect();
        (
            head,
            format!("{}; {}.", capitalize(&parts.join(", ")), channels),
        )
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

fn email_failures(activities: &[EmailActivity], now: f64) -> usize {
    activities
        .iter()
        .filter(|a| {
            a.processed_at as f64 >= now - DAY_MS
                && matches!(
                    a.outcome,
                    EmailActivityOutcome::Failed
                        | EmailActivityOutcome::Error
                        | EmailActivityOutcome::Partial
                )
        })
        .count()
}

#[component]
fn NeedsYou(items: Signal<Vec<Attention>>, loading: Signal<bool>) -> impl IntoView {
    let title = Signal::derive(move || {
        if items.with(Vec::is_empty) {
            "Nothing needs you".to_owned()
        } else {
            "Needs you".to_owned()
        }
    });
    view! {
        <Panel title=title aria_label="Needs you">
            {move || {
                if loading.get() && items.with(Vec::is_empty) {
                    return view! { <SkeletonRows count=2/> }.into_any();
                }
                let list = items.get();
                if list.is_empty() {
                    return view! {
                        <div class="rows">
                            <div class="row"><Status kind=StatusKind::Ok label="Tasks healthy"/></div>
                            <div class="row"><Status kind=StatusKind::Ok label="Email clean today"/></div>
                        </div>
                    }
                    .into_any();
                }
                view! {
                    <div class="rows">
                        {list.into_iter().map(|item| view! {
                            <Link to=item.href class="row">
                                <Status kind=item.kind label=item.label/>
                                <span class="row-end"><Glyph icon=Icon::ChevronRight size=IconSize::Small/></span>
                            </Link>
                        }).collect_view()}
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}

#[component]
fn UpNext() -> impl IntoView {
    let live = use_live_data();
    let now = use_now(1000);
    let rows = Memo::new(move |_| {
        live.snapshot.with(|s| {
            let Some(s) = s else {
                return (Vec::new(), Vec::new());
            };
            let (realtime, mut rest): (Vec<&TaskInfo>, Vec<&TaskInfo>) = s
                .tasks
                .iter()
                .partition(|t| cadence(t) == Cadence::Realtime);
            rest.sort_by(|a, b| {
                next_run_ms(a)
                    .unwrap_or(f64::MAX)
                    .total_cmp(&next_run_ms(b).unwrap_or(f64::MAX))
            });
            let rest = rest
                .into_iter()
                .take(5)
                .map(|t| {
                    (
                        t.name.clone(),
                        task_label(&t.name, t.display_name.as_deref()),
                        next_run_ms(t),
                        t.running,
                    )
                })
                .collect();
            let realtime = realtime.iter().filter_map(|t| period_ms(t)).collect();
            (rest, realtime)
        })
    });
    view! {
        <Panel title="Up next" head_end=ViewFn::from(|| view! { <Link to="/operations" class="textlink small">"Operations"</Link> })>
            <div class="rows">
                {move || rows.with(|(rest, _)| rest.clone()).into_iter().map(|(name, label, next, running)| {
                    let href = format!("/operations#inspect={}", encode_uri_component(&name));
                    view! {
                        <Link to=href class="row dense">
                            <span class="row-main"><span class="row-title">{label}</span></span>
                            <span class="row-end num">
                                {move || if running { "running".to_owned() } else { next.map(|n| format_next(n - now.get())).unwrap_or_else(|| "—".to_owned()) }}
                            </span>
                        </Link>
                    }
                }).collect_view()}
                {move || {
                    let periods = rows.with(|(_, r)| r.clone());
                    (!periods.is_empty()).then(|| {
                        let lo = periods.iter().copied().fold(f64::INFINITY, f64::min) / 1000.0;
                        let hi = periods.iter().copied().fold(0.0, f64::max) / 1000.0;
                        let range = if (hi - lo).abs() < 1.0 { format!("{lo:.0} s") } else { format!("{lo:.0}–{hi:.0} s") };
                        view! {
                            <Link to="/operations" class="row dense">
                                <span class="row-main"><span class="row-title dim">{format!("Realtime checks ×{} · {range}", periods.len())}</span></span>
                            </Link>
                        }
                    })
                }}
            </div>
        </Panel>
    }
}

#[component]
fn OfflineChips(streamers: Vec<OfflineStreamerView>) -> impl IntoView {
    let now = now_ms();
    view! {
        <div class="offline-chips">
            {streamers.into_iter().map(|s| {
                let last = s.last_ended_at.or(s.last_started_at).map(|t| format_relative_at(t as f64, now));
                view! {
                    <Link to=streamer_path(&s.id) class="offline-chip" title=last.clone().map(|l| format!("Last live {l}")).unwrap_or_default()>
                        <Avatar name=s.display_name.clone() size=24 presence=Presence::Offline image=s.avatar_url.clone()/>
                        <span>{s.display_name.clone()}</span>
                        {last.map(|l| view! { <span class="small dim">{l}</span> })}
                    </Link>
                }
            }).collect_view()}
        </div>
    }
}

#[component]
fn OnAir() -> impl IntoView {
    let live = use_live_data();
    let sort = RwSignal::new(LiveSort::Relevance);
    let order = use_live_order(live, sort.into());
    let lead = Memo::new(move |_| order.with(|o| o.first().cloned()));
    let others = Memo::new(move |_| order.with(|o| o.iter().skip(1).cloned().collect::<Vec<_>>()));
    let meta = Memo::new(move |_| on_air_meta(&live_list(live)));
    let offline = Memo::new(move |_| {
        let mut list: Vec<OfflineStreamerView> = live.snapshot.with(|s| {
            s.as_ref()
                .map(|s| {
                    s.streamers
                        .iter()
                        .filter_map(|v| match v {
                            StreamerView::Offline(o) => Some(o.clone()),
                            StreamerView::Live(_) => None,
                        })
                        .collect()
                })
                .unwrap_or_default()
        });
        list.sort_by_key(|o| {
            (
                o.tier != StreamerTier::Primary,
                std::cmp::Reverse(o.last_ended_at.unwrap_or(0)),
            )
        });
        list
    });
    let is_live = Signal::derive(move || lead.with(Option::is_some));
    let can_sort = Memo::new(move |_| order.with(Vec::len) > 1);
    let has_offline = Memo::new(move |_| offline.with(|list| !list.is_empty()));
    let offline_summary = Signal::derive(move || {
        offline.with(|list| {
            let primary = list
                .iter()
                .filter(|o| o.tier == StreamerTier::Primary)
                .count();
            format!(
                "{} offline · {primary} primary",
                plural(list.len(), "channel", "channels")
            )
        })
    });
    view! {
        <Panel stage=is_live aria_label="On air" class="on-air">
            <div class="panel-head">
                <h2 class="panel-title">
                    {move || if is_live.get() {
                        view! { <LiveTag/> }.into_any()
                    } else {
                        view! { <span>"On air"</span> }.into_any()
                    }}
                </h2>
                <span class="panel-meta">{move || is_live.get().then(|| meta.get())}</span>
                <span class="spacer"></span>
                {move || can_sort.get().then(|| view! {
                    <Segmented
                        options=Signal::derive(sort_options)
                        value=sort
                        on_change=Callback::new(move |v| sort.set(v))
                        aria_label="Sort live channels"
                        small=true
                    />
                })}
            </div>
            {move || match lead.get() {
                Some(id) => view! { <LeadStage id/> }.into_any(),
                None => view! { <EmptyState icon=Icon::Live message="Nobody is live right now." compact=true/> }.into_any(),
            }}
            <div class="rows" data-primary-rows="true">
                <For each=move || others.get() key=|id| id.clone() children=|id| view! { <LiveRow id/> }/>
            </div>
            // The disclosure stays mounted across snapshots so it keeps its
            // open state; only the chips re-render.
            {move || has_offline.get().then(|| view! {
                <div class="panel-foot">
                    <Disclosure summary=offline_summary>
                        {move || view! { <OfflineChips streamers=offline.get()/> }}
                    </Disclosure>
                </div>
            })}
        </Panel>
    }
}

#[component]
fn InboxPanel(
    emails: RwSignal<Option<Vec<EmailActivity>>>,
    error: RwSignal<Option<String>>,
) -> impl IntoView {
    let counts = Memo::new(move |_| {
        let now = now_ms();
        emails.with(|e| {
            e.as_ref().map(|e| {
                let today: Vec<&EmailActivity> = e
                    .iter()
                    .filter(|a| a.processed_at as f64 >= now - DAY_MS)
                    .collect();
                let processed = today
                    .iter()
                    .filter(|a| {
                        matches!(
                            a.outcome,
                            EmailActivityOutcome::Processed | EmailActivityOutcome::Partial
                        )
                    })
                    .count();
                let filtered = today
                    .iter()
                    .filter(|a| {
                        matches!(
                            a.outcome,
                            EmailActivityOutcome::Filtered
                                | EmailActivityOutcome::Skipped
                                | EmailActivityOutcome::NoMatches
                        )
                    })
                    .count();
                let failed = today
                    .iter()
                    .filter(|a| {
                        matches!(
                            a.outcome,
                            EmailActivityOutcome::Failed | EmailActivityOutcome::Error
                        )
                    })
                    .count();
                (processed, filtered, failed)
            })
        })
    });
    view! {
        <Panel title="Inbox" head_end=ViewFn::from(|| view! { <span class="panel-meta">"24 h"</span> <Link to="/emails" class="textlink small">"Email"</Link> })>
            {move || match (counts.get(), error.get()) {
                (Some((processed, filtered, failed)), _) => view! {
                    <div class="readouts" style="--cols: 3">
                        <Readout label="Processed" value=processed.to_string() size=ReadoutSize::M/>
                        <Readout label="Filtered" value=filtered.to_string() size=ReadoutSize::M/>
                        <Readout label="Failed" value=failed.to_string() size=ReadoutSize::M tone={if failed > 0 { Tone::Fault } else { Tone::Neutral }}/>
                    </div>
                }.into_any(),
                (None, Some(e)) => view! { <ErrorState title="Email activity could not be loaded" raw=e/> }.into_any(),
                (None, None) => view! { <div class="panel-body"><Skeleton kind=SkeletonKind::Readout/></div> }.into_any(),
            }}
        </Panel>
    }
}

#[component]
fn SpendPanel() -> impl IntoView {
    let costs = RwSignal::new(None::<CostsResponse>);
    let error = RwSignal::new(None::<String>);
    spawn_scoped(async move {
        match api::fetch_costs(CostRange::Days(30)).await {
            Ok(res) => costs.set(Some(res)),
            Err(e) => error.set(Some(e.message().to_owned())),
        }
    });
    view! {
        <Panel title="Spend" head_end=ViewFn::from(|| view! { <span class="panel-meta">"30 days"</span> <Link to="/costs" class="textlink small">"Costs"</Link> })>
            {move || match (costs.get(), error.get()) {
                (Some(c), _) => {
                    let max = c.daily.iter().map(|d| d.cost_cents).fold(0.0, f64::max).max(1e-9);
                    let top = c.by_feature.iter().max_by(|a, b| a.cost_cents.total_cmp(&b.cost_cents)).map(|f| f.feature.clone());
                    view! {
                        <div class="panel-body stack">
                            <Readout label="Last 30 days" value=format_cents(Some(c.summary.selected_cost_cents)).unwrap_or_default()>
                                {top.map(|t| view! { <span>"Largest: " {t}</span> })}
                            </Readout>
                            <div class="mini-bars" role="img" aria-label="Daily spend, last 30 days">
                                {c.daily.iter().map(|d| view! {
                                    <i style=format!("height: {:.1}%", d.cost_cents / max * 100.0) title=format!("{} · {}", d.date, format_cents(Some(d.cost_cents)).unwrap_or_default())></i>
                                }).collect_view()}
                            </div>
                        </div>
                    }.into_any()
                }
                (None, Some(e)) => view! { <ErrorState title="Costs could not be loaded" raw=e/> }.into_any(),
                (None, None) => view! { <div class="panel-body"><Skeleton kind=SkeletonKind::Readout/></div> }.into_any(),
            }}
        </Panel>
    }
}

#[component]
pub fn HomePage() -> impl IntoView {
    let live = use_live_data();
    let now = use_now(30_000);
    let emails = RwSignal::new(None::<Vec<EmailActivity>>);
    let email_error = RwSignal::new(None::<String>);
    let calendar = use_calendar_status();
    let pet_health = use_task_backed(omni_web_pages::pets::PET_TASK, api::fetch_pet_health);
    let newest_run = Memo::new(move |_| {
        live.snapshot
            .with(|s| s.as_ref().and_then(|s| s.runs.first()?.finished_at))
    });
    Effect::new(move |_| {
        newest_run.track();
        spawn_scoped(async move {
            match api::fetch_email_activity(None, Some(500)).await {
                Ok(res) => {
                    emails.set(Some(res.activities));
                    email_error.set(None);
                }
                Err(e) => email_error.set(Some(e.message().to_owned())),
            }
        });
    });

    let items = Memo::new(move |_| {
        let now = now.get();
        let failures = emails.with(|e| e.as_deref().map_or(0, |e| email_failures(e, now)));
        let personal = PersonalAttention {
            calendar: calendar.data.with(|c| {
                c.as_ref()
                    .is_some_and(|c| omni_web_pages::calendar::sync_needs_you(c, now))
            }),
            pets: pet_health
                .data
                .with(|h| h.as_ref().map_or(0, omni_web_pages::pets::attention_count)),
        };
        live.snapshot.with(|s| {
            attention_items(
                s.as_ref().map_or(&[][..], |s| &s.tasks),
                failures,
                personal,
                now,
            )
        })
    });
    let loading =
        Signal::derive(move || live.snapshot.with(Option::is_none) || emails.with(Option::is_none));
    let has_on_deck = Memo::new(move |_| {
        live.snapshot
            .with(|s| s.as_ref().is_some_and(|s| !s.on_deck.is_empty()))
    });
    let sentence = Memo::new(move |_| {
        let live_count = live_list(live).len();
        status_sentence(
            &items.get(),
            live_count,
            live.snapshot.with(Option::is_some),
        )
    });

    // Gate on loaded-ness only: rebuilding the page on every snapshot would
    // recreate On air and reset its sort.
    let loaded = Memo::new(move |_| live.snapshot.with(Option::is_some));

    view! {
        {move || {
            if !loaded.get() {
                return match live.error.get() {
                    Some(e) => view! {
                        <ErrorState
                            title="Home could not load"
                            detail="The dashboard snapshot is unavailable."
                            raw=e
                            retry=Callback::new(|()| { let _ = window().location().reload(); })
                            link=("Operations".to_owned(), "/operations".to_owned())
                            page=true
                        />
                    }.into_any(),
                    None => view! {
                        <div class="stack-lg">
                            <Skeleton kind=SkeletonKind::Title width="40%"/>
                            <Skeleton width="60%"/>
                            <SkeletonRows count=5/>
                        </div>
                    }.into_any(),
                };
            }
            view! {
                <PageHead
                    title=Signal::derive(move || sentence.get().0)
                    lede=Signal::derive(move || Some(sentence.get().1))
                    sentence=true
                />
                {move || live.error.get().map(|e| view! {
                    <p class="inline-note warn" role="status">{format!("Refresh failed ({e}); showing the last known state.")}</p>
                })}
                <div class="split home-top">
                    <OnAir/>
                    <div class="sticky-side">
                        <NeedsYou items=items.into() loading/>
                        <AgendaTile status=calendar/>
                        <UpNext/>
                    </div>
                </div>
                {move || has_on_deck.get().then(|| view! {
                    <section class="section">
                        <div class="section-head">
                            <h2 class="section-title">"On deck"</h2>
                            <span class="section-meta">{move || {
                                let n = live.snapshot.with(|s| s.as_ref().map_or(0, |s| s.on_deck.len()));
                                if n == 1 { "1 pick".to_owned() } else { format!("{n} picks") }
                            }}</span>
                            <div class="section-end"><Link to="/media" class="textlink small">"All picks"</Link></div>
                        </div>
                        <OnDeck items=Signal::derive(move || live.snapshot.with(|s| s.as_ref().map(|s| s.on_deck.clone()).unwrap_or_default()))/>
                    </section>
                })}
                <div class="grid-2 home-panels">
                    <InboxPanel emails error=email_error/>
                    <SpendPanel/>
                </div>
            }
            .into_any()
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentence_reads_quiet_or_counts_items() {
        let (head, line) = status_sentence(&[], 4, true);
        assert_eq!(head, "All quiet.");
        assert_eq!(
            line,
            "4 channels on air, every task healthy, nothing waiting on you."
        );
        let items = attention_items(&[], 2, PersonalAttention::default(), 0.0);
        let (head, line) = status_sentence(&items, 0, true);
        assert_eq!(head, "2 things need you.");
        assert_eq!(line, "2 emails failed today; nobody on air.");
        let personal = PersonalAttention {
            calendar: true,
            pets: 2,
        };
        let labels: Vec<String> = attention_items(&[], 0, personal, 0.0)
            .into_iter()
            .map(|i| i.label)
            .collect();
        assert_eq!(labels, ["1 calendar sync failing", "2 pet health alerts"]);
    }

    #[test]
    fn degraded_tasks_join_the_attention_list() {
        use omni_api::runs::{Run, RunStatus, RunTrigger};
        let task = TaskInfo {
            name: "Events".into(),
            display_name: None,
            schedule: String::new(),
            running: false,
            next_runs: Vec::new(),
            last_run: Some(Run {
                run_id: "r".into(),
                task_name: "Events".into(),
                trigger: RunTrigger::Schedule,
                scheduled_for: None,
                started_at: 0,
                finished_at: Some(1),
                status: RunStatus::Degraded,
                error: Some("upstream down".into()),
                summary: None,
            }),
        };
        let items = attention_items(&[task], 0, PersonalAttention::default(), 0.0);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "1 task degraded");
        assert_eq!(items[0].kind, StatusKind::Warn);
    }
}
