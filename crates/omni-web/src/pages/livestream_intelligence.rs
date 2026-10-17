//! What the intelligence pipeline is doing, why, and what it cost.
//! Refreshes every 10 s.

use std::time::Duration;

use leptos::prelude::*;
use omni_api::common::encode_uri_component;
use omni_api::intelligence::{
    EventKind, EventStatus, IntelligenceDetailsResponse, LivestreamEvent, LivestreamIntelligence,
    MetricMap, PipelineStatus, StageDiagnostic,
};
use omni_api::streamers::StreamerView;
use omni_web_kit::api;
use omni_web_kit::chrome::use_page_label;
use omni_web_kit::components::{
    ButtonLink, Disclosure, EmptyState, ErrorState, Icon, InlineNote, Meter, PageHead, Panel,
    Readout, ReadoutBand, ReadoutSize, SegOption, Segmented, ShowMoreButton, SkeletonRows, Status,
    StatusKind, Tone,
};
use omni_web_kit::hooks::{use_now, use_visible_poll};
use omni_web_kit::use_live_data;
use omni_web_kit::utils::format::{format_duration, format_relative};
use omni_web_kit::utils::js::{date_locale_time_string, js_round, number_string, to_fixed};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimelineFilter {
    Key,
    All,
    Voice,
    Alerts,
    Errors,
}

/// Timeline events shown before "Show more" (the page refreshes every 10 s,
/// so the limit outlives each re-render).
const EVENTS_PAGE: usize = 20;

const STAGES: [(&str, &str); 3] = [
    ("voice", "Voice detection"),
    ("summary", "Now summary"),
    ("alert", "Alerts"),
];
const FILTERS: [(TimelineFilter, &str); 5] = [
    (TimelineFilter::Key, "Key"),
    (TimelineFilter::All, "All"),
    (TimelineFilter::Voice, "Voice"),
    (TimelineFilter::Alerts, "Alerts"),
    (TimelineFilter::Errors, "Errors"),
];

fn money_from_cents(cents: f64) -> String {
    format!(
        "${}",
        to_fixed(cents / 100.0, if cents < 1.0 { 4 } else { 2 })
    )
}

fn status_str(status: PipelineStatus) -> &'static str {
    match status {
        PipelineStatus::Idle => "idle",
        PipelineStatus::Running => "running",
        PipelineStatus::Success => "success",
        PipelineStatus::Skipped => "skipped",
        PipelineStatus::Error => "error",
    }
}

fn event_status_str(status: EventStatus) -> &'static str {
    match status {
        EventStatus::Info => "info",
        EventStatus::Success => "success",
        EventStatus::Warning => "warning",
        EventStatus::Error => "error",
    }
}

fn event_kind_str(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Session => "session",
        EventKind::Metadata => "metadata",
        EventKind::Voice => "voice",
        EventKind::Summary => "summary",
        EventKind::Alert => "alert",
        EventKind::Feedback => "feedback",
        EventKind::Anomaly => "anomaly",
    }
}

fn stage_status(stage: Option<&StageDiagnostic>) -> String {
    match stage {
        None => "not run".to_owned(),
        Some(stage) if stage.eligible == Some(false) => "not eligible".to_owned(),
        Some(stage) => status_str(stage.status).replace('_', " "),
    }
}

/// "voicePrintScore" → "Voice Print Score".
pub fn metric_label(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let mut out = String::new();
    for (i, ch) in chars.iter().enumerate() {
        if i > 0 && chars[i - 1].is_ascii_lowercase() && ch.is_ascii_uppercase() {
            out.push(' ');
        }
        out.push(*ch);
    }
    let mut it = out.chars();
    match it.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), it.as_str()),
        None => out,
    }
}

fn metric_value(value: &Value) -> String {
    match value {
        Value::Number(n) => number_string(js_round(n.as_f64().unwrap_or(0.0) * 1000.0) / 1000.0),
        Value::String(s) => s.clone(),
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn stage_kind(stage: Option<&StageDiagnostic>) -> StatusKind {
    match stage {
        None => StatusKind::Idle,
        Some(s) if s.eligible == Some(false) => StatusKind::Idle,
        Some(s) => match s.status {
            PipelineStatus::Idle | PipelineStatus::Skipped => StatusKind::Idle,
            PipelineStatus::Running => StatusKind::Running,
            PipelineStatus::Success => StatusKind::Ok,
            PipelineStatus::Error => StatusKind::Fault,
        },
    }
}

fn event_tone(status: EventStatus) -> StatusKind {
    match status {
        EventStatus::Info => StatusKind::Info,
        EventStatus::Success => StatusKind::Ok,
        EventStatus::Warning => StatusKind::Warn,
        EventStatus::Error => StatusKind::Fault,
    }
}

fn metrics_list(metrics: &MetricMap) -> impl IntoView + use<> {
    let rows = metrics
        .iter()
        .map(|(key, value)| {
            view! {
                <dt>{metric_label(key)}</dt>
                <dd class="num">{metric_value(value)}</dd>
            }
        })
        .collect_view();
    view! { <dl class="kv">{rows}</dl> }
}

/// One pipeline stage. Reactive inside so the 1 s clock and 10 s refresh
/// update facts in place instead of re-creating the panel (which would close
/// its Metrics disclosure).
#[component]
fn StagePanel(
    label: &'static str,
    #[prop(into)] stage: Signal<Option<StageDiagnostic>>,
    #[prop(into)] now: Signal<f64>,
) -> impl IntoView {
    let kind = Signal::derive(move || stage.with(|s| stage_kind(s.as_ref())));
    let status = Signal::derive(move || stage.with(|s| stage_status(s.as_ref())));
    let detail = move || {
        stage.with(|s| {
            s.as_ref()
                .and_then(|s| s.detail.clone())
                .unwrap_or_else(|| "No diagnostic state recorded yet.".to_owned())
        })
    };
    let facts = move || {
        let now = now.get();
        let mut facts = Vec::new();
        stage.with(|s| {
            if let Some(s) = s {
                if let Some(f) = s.finished_at.filter(|f| *f != 0) {
                    facts.push(format!("last {}", format_relative(f as f64)));
                }
                if s.status == PipelineStatus::Running
                    && let Some(started) = s.started_at.filter(|v| *v != 0)
                {
                    facts.push(format!("running {}", format_duration(now - started as f64)));
                }
                if let Some(next) = s.next_at.filter(|n| *n != 0) {
                    facts.push(format!(
                        "{} {}",
                        if (next as f64) <= now { "due" } else { "next" },
                        format_relative(next as f64)
                    ));
                }
                if let Some(d) = s.duration_ms {
                    facts.push(format!("{} processing", format_duration(d as f64)));
                }
            }
        });
        (!facts.is_empty()).then(|| view! { <p class="small dim num">{facts.join(" · ")}</p> })
    };
    let metrics = Memo::new(move |_| {
        stage.with(|s| {
            s.as_ref()
                .and_then(|s| s.metrics.clone())
                .filter(|m| !m.is_empty())
        })
    });
    let has_metrics = Memo::new(move |_| metrics.with(Option::is_some));
    view! {
        <Panel title=label pad=true head_end=ViewFn::from(move || view! { <Status kind label=status/> })>
            <p class="small">{detail}</p>
            {facts}
            {move || has_metrics.get().then(|| view! {
                <Disclosure summary="Metrics" flush=true>
                    {move || metrics.with(|m| m.as_ref().map(metrics_list))}
                </Disclosure>
            })}
        </Panel>
    }
}

fn event_visible(event: &LivestreamEvent, filter: TimelineFilter) -> bool {
    match filter {
        TimelineFilter::All => true,
        TimelineFilter::Voice => event.kind == EventKind::Voice,
        TimelineFilter::Alerts => matches!(event.kind, EventKind::Alert | EventKind::Feedback),
        TimelineFilter::Errors => event.status == EventStatus::Error,
        TimelineFilter::Key => {
            event.kind != EventKind::Session || event.status != EventStatus::Info
        }
    }
}

#[component]
fn TimelineEvent(event: LivestreamEvent) -> impl IntoView {
    let mut facts = vec![event_kind_str(event.kind).replace('_', " ")];
    if let Some(d) = event.duration_ms {
        facts.push(format!("{} processing", format_duration(d as f64)));
    }
    if let Some(c) = event.cost_cents {
        facts.push(if c == 0.0 {
            "$0 local".to_owned()
        } else {
            money_from_cents(c)
        });
    }
    view! {
        <li class="timeline-event">
            <span class="timeline-time num dim">
                {date_locale_time_string(event.created_at as f64, &[("hour", "numeric"), ("minute", "2-digit")])}
            </span>
            <span class="timeline-mark"><Status kind=event_tone(event.status) dot_only=true label=event_status_str(event.status)/></span>
            <div class="timeline-body">
                <div class="row-title">{event.title.clone()}</div>
                {event.detail.clone().filter(|d| !d.is_empty()).map(|d| view! { <p class="small">{d}</p> })}
                <div class="small dim">{facts.join(" · ")}</div>
                {event.metrics.as_ref().filter(|m| !m.is_empty()).map(|m| {
                    let list = metrics_list(m);
                    view! { <Disclosure summary="Evidence" flush=true>{list}</Disclosure> }
                })}
            </div>
        </li>
    }
}

fn stage_of(details: &IntelligenceDetailsResponse, key: &str) -> Option<StageDiagnostic> {
    let value = details.diagnostics.as_ref()?.stages.get(key)?;
    serde_json::from_value(value.clone()).ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Timeline,
    Diagnostics,
}

#[component]
fn Evidence(intelligence: Option<LivestreamIntelligence>) -> impl IntoView {
    let Some(intel) = intelligence else {
        return ().into_any();
    };
    let transcript = match &intel.summary {
        Some(summary) => view! {
            <h3>{summary.topic.clone()}</h3>
            <p class="prose small transcript-excerpt">{summary.transcript_excerpt.clone()}</p>
            <p class="small dim num">
                {format!(
                    "{}% confidence · {} s window",
                    number_string(js_round(summary.confidence * 100.0)),
                    number_string(summary.window_seconds)
                )}
                {summary.speakers.clone().map(|speakers| format!(" · {speakers}"))}
            </p>
        }
        .into_any(),
        None => {
            view! { <p class="dim">"No transcript-backed summary for this session."</p> }.into_any()
        }
    };
    let surge = match &intel.trend {
        Some(trend) => {
            let headline = if trend.anomalous {
                "Confirmed".to_owned()
            } else {
                trend
                    .suppression_reason
                    .clone()
                    .unwrap_or_else(|| "No unusual rise".to_owned())
            };
            let opt =
                |v: Option<f64>| v.map_or_else(|| "—".to_owned(), |v| number_string(js_round(v)));
            view! {
                <h3 class=if trend.anomalous { "text-signal" } else { "" }>{headline}</h3>
                <dl class="kv">
                    <dt>"Platform now / base"</dt>
                    <dd class="num">{format!("{} / {}", opt(trend.current_viewers), opt(trend.baseline_viewers))}</dd>
                    <dt>"DGG now / base"</dt>
                    <dd class="num">{format!("{} / {}", opt(trend.current_dgg_viewers), opt(trend.baseline_dgg_viewers))}</dd>
                    <dt>"Typical peak"</dt>
                    <dd class="num">{opt(trend.typical_peak_viewers)}</dd>
                    <dt>"Baseline samples"</dt>
                    <dd class="num">{number_string(trend.baseline_samples.unwrap_or(0.0))}</dd>
                    <dt>"Confirmations"</dt>
                    <dd class="num">{format!("{} of 2", number_string(trend.candidate_observations.unwrap_or(0.0).min(2.0)))}</dd>
                </dl>
            }
            .into_any()
        }
        None => {
            view! { <p class="dim">"No viewer baseline collected for this session."</p> }.into_any()
        }
    };
    view! {
        <div class="grid-2 intel-evidence">
            <Panel title="Latest transcript window" pad=true>{transcript}</Panel>
            <Panel title="Viewer surge" pad=true>{surge}</Panel>
        </div>
    }
    .into_any()
}

#[component]
pub fn LivestreamIntelligencePage(#[prop(into)] streamer_id: String) -> impl IntoView {
    let live = use_live_data();
    let now = use_now(1_000);
    let details = RwSignal::new(None::<IntelligenceDetailsResponse>);
    let error = RwSignal::new(None::<String>);
    let filter = RwSignal::new(TimelineFilter::Key);
    let event_limit = RwSignal::new(EVENTS_PAGE);
    let view_mode = RwSignal::new(View::Timeline);
    let id = streamer_id.clone();
    let streamer = Memo::new(move |_| {
        live.snapshot.with(|s| {
            s.as_ref()
                .and_then(|s| s.streamers.iter().find(|x| x.id() == id).cloned())
        })
    });
    let display = Memo::new(move |_| {
        streamer.with(|s| {
            s.as_ref().map(|s| match s {
                StreamerView::Live(l) => l.display_name.clone(),
                StreamerView::Offline(o) => o.display_name.clone(),
            })
        })
    });

    let fetch_id = streamer_id.clone();
    let reload = use_visible_poll(
        Signal::derive(String::new),
        move || {
            let id = fetch_id.clone();
            async move { api::fetch_livestream_intelligence_details(&id, 100).await }
        },
        move |next| {
            // Notify only on change so an unchanged poll re-renders nothing.
            if details.with_untracked(|d| d.as_ref() != Some(&next)) {
                details.set(Some(next));
            }
            if error.with_untracked(Option::is_some) {
                error.set(None);
            }
        },
        move |e: api::ApiClientError| error.set(Some(e.message().to_owned())),
        Duration::from_secs(10),
    );
    Effect::new(move |_| {
        if let Some(name) = display.get() {
            document().set_title(&format!("{name} Intelligence · Omni"));
        }
    });
    use_page_label(move || Some("Intelligence".to_owned()));

    // The page closure reads only this phase; every section below reads its
    // own memo, so snapshots (every few seconds), the 10 s poll and the 1 s
    // clock update in place without resetting disclosures or scroll.
    let phase = Memo::new(move |_| {
        let has_details = details.with(Option::is_some);
        if live.snapshot.with(Option::is_none) || (!has_details && error.with(Option::is_none)) {
            Phase::Loading
        } else if streamer.with(Option::is_none) {
            Phase::Unknown
        } else if !has_details {
            Phase::Failed
        } else {
            Phase::Ready
        }
    });
    let is_live = Memo::new(move |_| streamer.with(|s| matches!(s, Some(StreamerView::Live(_)))));
    let intelligence =
        Memo::new(move |_| details.with(|d| d.as_ref().and_then(|d| d.intelligence.clone())));
    let events_all = Memo::new(move |_| {
        details.with(|d| d.as_ref().map(|d| d.events.clone()).unwrap_or_default())
    });
    let filter_options = Signal::derive(move || {
        events_all.with(|events| {
            FILTERS
                .iter()
                .map(|(f, label)| {
                    let n = events.iter().filter(|e| event_visible(e, *f)).count();
                    SegOption::new(*f, *label).with_count(n)
                })
                .collect::<Vec<_>>()
        })
    });
    let visible = Memo::new(move |_| {
        let selected = filter.get();
        events_all.with(|events| {
            events
                .iter()
                .filter(|e| event_visible(e, selected))
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let no_events = Memo::new(move |_| visible.with(Vec::is_empty));
    let remaining = Memo::new(move |_| visible.with(Vec::len).saturating_sub(event_limit.get()));
    let has_more = Memo::new(move |_| remaining.get() > 0);
    let stages = STAGES.map(|(key, label)| {
        let stage = Memo::new(move |_| details.with(|d| d.as_ref().and_then(|d| stage_of(d, key))));
        (label, stage)
    });

    let back_to = format!("/streamers/{}", encode_uri_component(&streamer_id));
    move || {
        match phase.get() {
        Phase::Loading => {
            view! { <SkeletonRows count=8 label="Loading intelligence"/> }.into_any()
        }
        Phase::Unknown => view! {
            <ErrorState title="Unknown streamer" detail="This channel is not being monitored." link=("All streamers".to_owned(), "/live".to_owned()) page=true/>
        }
        .into_any(),
        Phase::Failed => view! {
            <ErrorState title="Intelligence could not load" raw=Signal::derive(move || error.get().unwrap_or_default()) retry=reload page=true/>
        }
        .into_any(),
        Phase::Ready => view! {
            <PageHead
                title=Signal::derive(move || format!("{} intelligence", display.get().unwrap_or_default()))
                lede="What the pipeline is doing, why it made each decision, and what it cost."
                actions=ViewFn::from({
                    let back_to = back_to.clone();
                    move || view! { <ButtonLink to=back_to.clone() icon=Icon::Live>"Streamer"</ButtonLink> }
                })
            >
                <p class="small muted">{move || if is_live.get() { "Showing the live session" } else { "Showing the last session" }}</p>
            </PageHead>
            {move || error.get().map(|e| view! { <InlineNote tone=Tone::Warn role="alert">{format!("Latest refresh failed: {e}")}</InlineNote> })}
            <div class="summary-bar sticky-summary" aria-label="Intelligence health">
                {move || details.with(|d| d.as_ref().map(summary_band))}
            </div>
            {move || view! { <Evidence intelligence=intelligence.get()/> }}
            <div class="toolbar">
                <Segmented
                    options=Signal::derive(|| vec![SegOption::new(View::Timeline, "Timeline"), SegOption::new(View::Diagnostics, "Diagnostics")])
                    value=view_mode
                    on_change=Callback::new(move |v| view_mode.set(v))
                    aria_label="Intelligence view"
                />
                {move || (view_mode.get() == View::Timeline).then(|| view! {
                    <Segmented options=filter_options value=filter on_change=Callback::new(move |f| {
                        filter.set(f);
                        event_limit.set(EVENTS_PAGE);
                    }) aria_label="Timeline filter" small=true fill=true/>
                })}
            </div>
            {move || match view_mode.get() {
                View::Timeline => view! {
                    <Panel title="Decision timeline">
                        {move || if no_events.get() {
                            view! { <EmptyState compact=true message="No events match this filter yet."/> }.into_any()
                        } else {
                            view! {
                                <ol class="timeline">
                                    <For
                                        each=move || visible.with(|v| v.iter().take(event_limit.get()).cloned().collect::<Vec<_>>())
                                        key=|e| (e.event_id.clone(), event_status_str(e.status), e.title.clone(), e.detail.clone(), e.duration_ms)
                                        children=|event| view! { <TimelineEvent event/> }
                                    />
                                </ol>
                                {move || has_more.get().then(|| view! {
                                    <ShowMoreButton
                                        remaining
                                        noun="events"
                                        on_click=Callback::new(move |()| event_limit.update(|n| *n += EVENTS_PAGE))
                                    />
                                })}
                            }
                            .into_any()
                        }}
                    </Panel>
                }
                .into_any(),
                View::Diagnostics => view! {
                    <div class="grid-3">
                        {stages.into_iter().map(|(label, stage)| view! { <StagePanel label stage now/> }).collect_view()}
                    </div>
                }
                .into_any(),
            }}
        }
        .into_any(),
    }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Loading,
    Unknown,
    Failed,
    Ready,
}

/// The sticky health band; stateless, so it re-renders per refresh.
fn summary_band(data: &IntelligenceDetailsResponse) -> impl IntoView + use<> {
    let runtime = data.runtime.clone();
    let queue_total = runtime.as_ref().map_or(0, |r| {
        [r.queues.capture, r.queues.speech, r.queues.llm]
            .iter()
            .map(|q| q.running + q.queued)
            .sum::<u64>()
    });
    let has_stage_error = data.diagnostics.as_ref().is_some_and(|d| {
        d.stages
            .values()
            .any(|stage| stage.get("status").and_then(Value::as_str) == Some("error"))
    });
    let (pipeline_word, pipeline_kind) = match (&runtime, has_stage_error) {
        (None, _) => ("Unavailable", StatusKind::Idle),
        (Some(_), true) => ("Needs attention", StatusKind::Fault),
        (Some(_), false) => ("Healthy", StatusKind::Ok),
    };
    let budget = runtime
        .as_ref()
        .map(|r| (r.budget.spent_cents, r.budget.limit_cents));
    let chapters = data.intelligence.as_ref().map_or(0, |i| i.chapters.len());
    let event_count = data.events.len();
    view! {
        <ReadoutBand cols=4>
            <Readout label="Pipeline" value=pipeline_word size=ReadoutSize::M class="word" tone={if has_stage_error { Tone::Fault } else { Tone::Neutral }}>
                <Status kind=pipeline_kind label=if has_stage_error {
                    "A stage failed".to_owned()
                } else if queue_total == 0 {
                    "Queues clear".to_owned()
                } else {
                    format!("{queue_total} queued or running")
                }/>
            </Readout>
            <Readout
                label="Voice model"
                value=if runtime.as_ref().is_some_and(|r| r.voiceprint_loaded) { "Ready" } else { "Unavailable" }
                size=ReadoutSize::M
                class="word"
            >
                {runtime.as_ref().map_or_else(
                    || "No runtime connection".to_owned(),
                    |r| format!("{} of {} live targets", r.active_voice_target_count, r.active_stream_count),
                )}
            </Readout>
            <Readout
                label="Monthly budget"
                value=budget.map_or_else(|| "—".to_owned(), |(spent, _)| money_from_cents(spent))
                size=ReadoutSize::M
            >
                {budget.map(|(spent, limit)| view! {
                    <Meter value=spent max=limit.max(0.01) tone={if spent >= limit { Tone::Fault } else { Tone::Neutral }} label=format!("of {}", money_from_cents(limit))/>
                    <span class="small dim">{format!("of {}", money_from_cents(limit))}</span>
                })}
            </Readout>
            <Readout label="Timeline" value=format!("{event_count}") size=ReadoutSize::M>
                {format!("events · {chapters} chapters")}
            </Readout>
        </ReadoutBand>
    }
}

#[cfg(test)]
mod tests {
    use super::metric_label;

    #[test]
    fn metric_labels_split_camel_case() {
        assert_eq!(metric_label("voicePrintScore"), "Voice Print Score");
        assert_eq!(metric_label("x"), "X");
    }
}
