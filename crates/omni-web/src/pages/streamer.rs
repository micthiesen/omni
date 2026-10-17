//! One monitored streamer: hero (leading with the live topic and summary),
//! readout Stage, daily peak chart, streams by week, and the session
//! chapters, Baseline, Bindings and Records panels.

use std::collections::HashMap;

use leptos::prelude::*;
use omni_api::common::encode_uri_component;
use omni_api::intelligence::{
    FeedbackVerdict, LivestreamChapter, LivestreamIntelligence, PresenceState,
};
use omni_api::streamers::{
    LiveStreamerView, StreamSessionView, StreamerMetricsResponse, StreamerTier, StreamerView,
};
use omni_web_kit::api;
use omni_web_kit::charts::{BarChart, BarPoint, BarSeries};
use omni_web_kit::chrome::use_page_label;
use omni_web_kit::components::streamers::{
    clean_stream_title, live_read, platform_label, streamer_intelligence, viewer_number,
};
use omni_web_kit::components::{
    Avatar, ButtonLink, ButtonVariant, Chip, Delta, Disclosure, EmptyState, ErrorState, Icon,
    LiveTag, Meter, Panel, PlatformIcon, Presence, Readout, ReadoutSize, SegOption, Segmented,
    ShowMoreButton, Skeleton, SkeletonKind, SkeletonRows, Sparkline, Tag, Tone,
};
use omni_web_kit::hooks::use_now;
use omni_web_kit::live::use_live_data;
use omni_web_kit::task::{spawn_detached, spawn_scoped};
use omni_web_kit::utils::format::{
    format_compact_number, format_date_only, format_duration, format_relative, format_uptime,
};
use omni_web_kit::utils::js::{
    civil_from_days, date_locale_date_string, date_locale_time_string, days_from_civil, js_round,
    local_date_ms, local_day_index, locale_number, now_ms, number_string, week_start,
};

use crate::pages::live::watch_label;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Range {
    Days30,
    Days90,
    All,
}

impl Range {
    fn days(self) -> Option<i64> {
        match self {
            Range::Days30 => Some(30),
            Range::Days90 => Some(90),
            Range::All => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Range::Days30 => "30D",
            Range::Days90 => "90D",
            Range::All => "All",
        }
    }
}

/// UTC `YYYY-MM-DD` of day index `days` since the epoch.
fn date_string(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn parse_date(date: &str) -> Option<(i64, i64, i64)> {
    let mut parts = date.split('-').map(|p| p.parse::<i64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// `d.setDate(d.getDate() - days); d.toISOString().slice(0, 10)`: shift the
/// local calendar date, then take the UTC date.
fn days_ago(days: i64) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let now = js_sys::Date::new_0();
        let shifted = js_sys::Date::new_with_year_month_day_hr_min_sec_milli(
            now.get_full_year(),
            now.get_month() as i32,
            now.get_date() as i32 - days as i32,
            now.get_hours() as i32,
            now.get_minutes() as i32,
            now.get_seconds() as i32,
            now.get_milliseconds() as i32,
        );
        String::from(shifted.to_iso_string())
            .chars()
            .take(10)
            .collect()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        date_string((now_ms() / 86_400_000.0).floor() as i64 - days)
    }
}

#[derive(Clone, Debug, PartialEq)]
struct DayPoint {
    date: String,
    max_viewers: i64,
    streamed: bool,
}

/// Sparse daily buckets expanded into a continuous series so off days show
/// as gaps rather than being compressed away.
fn build_day_series(metrics: &StreamerMetricsResponse, range: Range) -> Vec<DayPoint> {
    let by_date: HashMap<&str, i64> = metrics
        .daily_buckets
        .iter()
        .map(|b| (b.date.as_str(), b.max_viewers))
        .collect();
    let Some(first_bucket) = metrics.daily_buckets.first().map(|b| b.date.clone()) else {
        return Vec::new();
    };
    let range_start = match range.days() {
        None => first_bucket.clone(),
        Some(days) => days_ago(days - 1),
    };
    // Never start before tracking began.
    let start = if range_start < first_bucket {
        first_bucket
    } else {
        range_start
    };
    let today = date_string((now_ms() / 86_400_000.0).floor() as i64);
    let Some((y, m, d)) = parse_date(&start) else {
        return Vec::new();
    };
    let mut cursor = days_from_civil(y, m, d);
    let mut series = Vec::new();
    loop {
        let date = date_string(cursor);
        if date > today {
            break;
        }
        let max = by_date.get(date.as_str()).copied();
        series.push(DayPoint {
            date,
            max_viewers: max.unwrap_or(0),
            streamed: max.is_some(),
        });
        cursor += 1;
    }
    series
}

fn local_day_ms(date: &str) -> f64 {
    parse_date(date).map_or(f64::NAN, |(y, m, d)| {
        local_date_ms(y as i32, m as i32, d as i32)
    })
}

fn format_day_tick(date: &str) -> String {
    date_locale_date_string(
        local_day_ms(date),
        &[("month", "short"), ("day", "numeric")],
    )
}

fn format_day_full(date: &str) -> String {
    date_locale_date_string(
        local_day_ms(date),
        &[
            ("weekday", "short"),
            ("month", "short"),
            ("day", "numeric"),
            ("year", "numeric"),
        ],
    )
}

fn format_session_time(timestamp: i64) -> String {
    date_locale_time_string(
        timestamp as f64,
        &[("hour", "numeric"), ("minute", "2-digit")],
    )
}

const WEEKS_SHOWN: usize = 4;
const TYPICAL_SESSIONS: usize = 10;

/// Median peak of the most recent completed sessions.
pub fn typical_peak(sessions: &[StreamSessionView]) -> Option<f64> {
    let mut peaks: Vec<i64> = sessions
        .iter()
        .take(TYPICAL_SESSIONS)
        .map(|s| s.peak_viewers)
        .filter(|p| *p > 0)
        .collect();
    if peaks.is_empty() {
        return None;
    }
    peaks.sort_unstable();
    let mid = peaks.len() / 2;
    Some(if peaks.len().is_multiple_of(2) {
        (peaks[mid - 1] + peaks[mid]) as f64 / 2.0
    } else {
        peaks[mid] as f64
    })
}

/// One week of sessions (newest first).
#[derive(Clone, Debug, PartialEq)]
pub struct Week {
    pub start_day: i64,
    pub sessions: Vec<StreamSessionView>,
}

impl Week {
    fn hours(&self) -> f64 {
        self.sessions
            .iter()
            .map(|s| s.duration_ms as f64)
            .sum::<f64>()
            / 3_600_000.0
    }

    fn peak(&self) -> i64 {
        self.sessions
            .iter()
            .map(|s| s.peak_viewers)
            .max()
            .unwrap_or(0)
    }
}

/// Newest-first sessions grouped by local Monday-start week.
pub fn group_by_week(sessions: &[StreamSessionView], day_of: impl Fn(i64) -> i64) -> Vec<Week> {
    let mut weeks: Vec<Week> = Vec::new();
    for session in sessions {
        let start = week_start(day_of(session.started_at));
        match weeks.last_mut() {
            Some(week) if week.start_day == start => week.sessions.push(session.clone()),
            _ => weeks.push(Week {
                start_day: start,
                sessions: vec![session.clone()],
            }),
        }
    }
    weeks
}

fn week_label(start_day: i64, today: i64) -> String {
    let this_week = week_start(today);
    if start_day == this_week {
        return "This week".to_owned();
    }
    if start_day == this_week - 7 {
        return "Last week".to_owned();
    }
    let (y, m, d) = civil_from_days(start_day);
    format!(
        "Week of {}",
        date_locale_date_string(
            local_date_ms(y as i32, m as i32, d as i32),
            &[("month", "short"), ("day", "numeric")]
        )
    )
}

fn range_options() -> Vec<SegOption<Range>> {
    [Range::Days30, Range::Days90, Range::All]
        .into_iter()
        .map(|r| SegOption::new(r, r.label()))
        .collect()
}

#[component]
fn ViewerChart(
    metrics: StreamerMetricsResponse,
    #[prop(into)] typical: Signal<Option<f64>>,
) -> impl IntoView {
    let range = RwSignal::new(Range::Days90);
    let record = metrics.all_time_max;
    let series = Memo::new(move |_| build_day_series(&metrics, range.get()));
    let meta = Signal::derive(move || {
        series.with(|s| {
            let streamed = s.iter().filter(|p| p.streamed).count();
            Some(format!("{streamed} of {} days streamed", s.len()))
        })
    });
    let today = date_string(local_day_index(now_ms()));
    let chart = move || {
        let points = series.get();
        if points.iter().all(|p| !p.streamed) {
            return view! { <EmptyState compact=true message="No streams in this range."/> }
                .into_any();
        }
        let bars: Vec<BarPoint> = points
            .iter()
            .map(|p| BarPoint {
                x: p.date.clone(),
                values: vec![p.max_viewers as f64],
            })
            .collect();
        let marks: Vec<Option<String>> = points
            .iter()
            .map(|p| {
                if p.streamed && p.max_viewers == record && record > 0 {
                    Some("bar-record".to_owned())
                } else if p.date == today {
                    Some("bar-today".to_owned())
                } else {
                    None
                }
            })
            .collect();
        let tip_points = points.clone();
        let tooltip = Callback::new(move |index: usize| {
            let Some(point) = tip_points.get(index) else {
                return ().into_any();
            };
            view! {
                <div class="chart-tooltip-label">{format_day_full(&point.date)}</div>
                <div class="chart-tooltip-value num">
                    {if point.streamed { locale_number(point.max_viewers as f64) } else { "No stream".to_owned() }}
                </div>
                {(point.streamed && point.max_viewers == record).then(|| view! { <div class="text-signal">"All-time record"</div> })}
            }
            .into_any()
        });
        let label = format!("Daily peak viewers, {} days", points.len());
        view! {
            <div class="chart-container">
                <BarChart
                    data=Signal::stored(bars)
                    series=Signal::stored(vec![BarSeries { key: "maxViewers".into(), color: "var(--bar)".into() }])
                    x_tick=Callback::new(|date: String| format_day_tick(&date))
                    y_tick=Callback::new(format_compact_number)
                    tooltip
                    radius=2.0
                    bar_class=Callback::new(move |i: usize| marks.get(i).cloned().flatten())
                    reference=Signal::derive(move || typical.get().map(|t| (t, format!("typical {}", format_compact_number(t)))))
                    label=label
                />
            </div>
            <div class="chart-legend">
                <span class="chart-legend-item"><i class="chart-tooltip-swatch swatch-bar"></i>"Daily peak"</span>
                <span class="chart-legend-item"><i class="chart-tooltip-swatch swatch-record"></i>"Record"</span>
                <span class="chart-legend-item"><i class="chart-tooltip-swatch swatch-today"></i>"Today"</span>
            </div>
        }
        .into_any()
    };
    view! {
        <Panel
            title="Daily peak viewers"
            head_end=ViewFn::from(move || view! {
                <span class="panel-meta">{move || meta.get()}</span>
                <Segmented options=Signal::derive(range_options) value=range on_change=Callback::new(move |r| range.set(r)) aria_label="Chart range" small=true/>
            })
            pad=true
        >
            {chart}
        </Panel>
    }
}

#[component]
fn SessionRow(
    session: StreamSessionView,
    longest: f64,
    record: i64,
    typical: Option<f64>,
) -> impl IntoView {
    let day = local_day_index(session.started_at as f64);
    let (y, m, d) = civil_from_days(day);
    let weekday = date_locale_date_string(
        local_date_ms(y as i32, m as i32, d as i32),
        &[("weekday", "short")],
    );
    let is_record = record > 0 && session.peak_viewers == record;
    let strong = typical.is_some_and(|t| session.peak_viewers as f64 >= 1.3 * t);
    let peak_class = if is_record {
        "row-value readout-m text-signal"
    } else if strong {
        "row-value readout-m strong"
    } else {
        "row-value readout-m"
    };
    view! {
        <div class="row session-row">
            <span class="date-block" aria-hidden="true">
                <span class="date-day num">{d}</span>
                <span class="date-dow">{weekday}</span>
            </span>
            <span class="row-main">
                <span class="row-title truncate" title=session.title.clone()>
                    {if session.title.is_empty() { "Untitled stream".to_owned() } else { session.title.clone() }}
                </span>
                <span class="row-sub parts">
                    <Meter value=session.duration_ms as f64 max=longest width=64 label=format!("Length {}", format_duration(session.duration_ms as f64))/>
                    <span class="num">{format_session_time(session.started_at)}</span>
                    <span class="num dim">{format_duration(session.duration_ms as f64)}</span>
                    <PlatformIcon platform=session.platform.clone() size=12/>
                </span>
            </span>
            <span class="row-end">
                <span class=peak_class title="Session peak">
                    {if session.peak_viewers > 0 { locale_number(session.peak_viewers as f64) } else { "—".to_owned() }}
                    {is_record.then_some(" ★")}
                </span>
            </span>
        </div>
    }
}

#[component]
fn Streams(
    sessions: Vec<StreamSessionView>,
    record: i64,
    typical: Option<f64>,
    #[prop(into)] live: Signal<Option<LiveStreamerView>>,
) -> impl IntoView {
    let today = local_day_index(now_ms());
    let weeks = group_by_week(&sessions, |ms| local_day_index(ms as f64));
    let longest = sessions
        .iter()
        .map(|s| s.duration_ms)
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let total_weeks = weeks.len();
    let shown = RwSignal::new(WEEKS_SHOWN);
    let weeks = StoredValue::new(weeks);
    let older = Signal::derive(move || {
        weeks.with_value(|w| {
            w.iter()
                .skip(shown.get())
                .map(|w| w.sessions.len())
                .sum::<usize>()
        })
    });
    let now = use_now(1000);
    view! {
        <Panel title="Streams" head_end=ViewFn::from(move || view! { <span class="panel-meta">{format!("{} tracked", sessions.len())}</span> })>
            {move || live.get().map(|l| view! {
                <div class="row session-row live-session">
                    <span class="date-block" aria-hidden="true"><LiveTag/></span>
                    <span class="row-main">
                        <span class="row-title truncate">{l.title.clone()}</span>
                        <span class="row-sub num">{format!("started {} · {}", format_session_time(l.started_at), format_uptime(now.get() - l.started_at as f64))}</span>
                    </span>
                    <span class="row-end"><span class="row-value readout-m">{locale_number(l.max_viewer_count as f64)}</span></span>
                </div>
            })}
            {move || weeks.with_value(|w| w.iter().take(shown.get()).cloned().collect::<Vec<_>>()).into_iter().map(|week| {
                let label = week_label(week.start_day, today);
                view! {
                    <div class="group-head">
                        <span>{label}</span>
                        <span class="num dim truncate">
                            {format!("{} stream{} · {:.1} h · peak {}", week.sessions.len(), if week.sessions.len() == 1 { "" } else { "s" }, week.hours(), format_compact_number(week.peak() as f64))}
                        </span>
                    </div>
                    <div class="rows">
                        {week.sessions.into_iter().map(|session| view! { <SessionRow session longest record typical/> }).collect_view()}
                    </div>
                }
            }).collect_view()}
            {move || (shown.get() < total_weeks).then(|| view! {
                <div class="panel-foot">
                    <ShowMoreButton remaining=older noun="older streams" on_click=Callback::new(move |()| shown.update(|n| *n += WEEKS_SHOWN))/>
                </div>
            })}
        </Panel>
    }
}

/// Session topics shown before the rest collapse under "earlier".
const RECENT_CHAPTERS: usize = 4;

#[component]
fn ChapterItem(chapter: LivestreamChapter) -> impl IntoView {
    view! {
        <li class="topic">
            <span class="label num">{date_locale_time_string(chapter.started_at, &[("hour", "numeric"), ("minute", "2-digit")])}</span>
            <h3>{chapter.title}</h3>
            <p>{chapter.summary}</p>
        </li>
    }
}

#[component]
fn NowPanel(
    streamer_id: String,
    #[prop(into)] intelligence: Signal<Option<LivestreamIntelligence>>,
    is_live: bool,
) -> impl IntoView {
    let submitted = RwSignal::new(false);
    let alert_id = Memo::new(move |_| {
        intelligence.with(|i| {
            i.as_ref()
                .and_then(|i| i.latest_alert.as_ref().map(|a| a.alert_id.clone()))
        })
    });
    Effect::new(move |_| {
        alert_id.track();
        submitted.set(false);
    });
    let id = StoredValue::new(streamer_id);
    let submit = move |verdict: FeedbackVerdict| {
        let Some(alert) = alert_id.get_untracked() else {
            return;
        };
        submitted.set(true);
        let id = id.get_value();
        spawn_detached(async move {
            if api::submit_livestream_feedback(&id, &alert, verdict, None)
                .await
                .is_err()
            {
                submitted.set(false);
            }
        });
    };
    let title = if is_live {
        "This session"
    } else {
        "Last session"
    };
    // Each part reads its own memo: intelligence changes with every viewer
    // trend update, and rebuilding the panel would close the transcript
    // disclosure and drop focus from the feedback chips.
    let present = Memo::new(move |_| intelligence.with(Option::is_some));
    let chapters = Memo::new(move |_| {
        intelligence.with(|i| {
            i.as_ref()
                .map(|i| i.chapters.iter().rev().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let summary =
        Memo::new(move |_| intelligence.with(|i| i.as_ref().and_then(|i| i.summary.clone())));
    let alert =
        Memo::new(move |_| intelligence.with(|i| i.as_ref().and_then(|i| i.latest_alert.clone())));
    view! {
        <Panel title=title pad=true>
            {move || if !present.get() {
                view! { <p class="dim">{if is_live { "No live read yet." } else { "No session summary." }}</p> }.into_any()
            } else {
                view! {
                    <div class="stack">
                        {move || {
                            let list = chapters.get();
                            if list.is_empty() {
                                return None;
                            }
                            let earlier = list.len().saturating_sub(RECENT_CHAPTERS);
                            let mut list = list.into_iter();
                            let recent: Vec<_> = list.by_ref().take(RECENT_CHAPTERS).collect();
                            let older: Vec<_> = list.collect();
                            Some(view! {
                                <ol class="topic-list" aria-label="Topics this session, latest first">
                                    {recent.into_iter().map(|c| view! { <ChapterItem chapter=c/> }).collect_view()}
                                </ol>
                                {(earlier > 0).then(|| view! {
                                    <Disclosure summary=format!("{earlier} earlier {}", if earlier == 1 { "topic" } else { "topics" }) flush=true>
                                        <ol class="topic-list">
                                            {older.into_iter().map(|c| view! { <ChapterItem chapter=c/> }).collect_view()}
                                        </ol>
                                    </Disclosure>
                                })}
                            })
                        }}
                        {move || intelligence.get().map(|i| {
                            let confidence = i.summary.as_ref().map(|s| s.confidence);
                            view! {
                                <div class="cluster">
                                    {confidence.map(|c| view! { <Tag>{format!("confidence {}%", number_string(js_round(c * 100.0)))}</Tag> })}
                                    <Tag tone=Tone::Info>{format!("relevance {}", number_string(js_round(i.relevance_score * 10.0) / 10.0))}</Tag>
                                    {i.destiny_presence.as_ref().filter(|p| p.state == PresenceState::Confirmed).map(|p| view! {
                                        <Tag tone=Tone::Signal>{format!("Destiny {}%", number_string(js_round(p.confidence * 100.0)))}</Tag>
                                    })}
                                </div>
                            }
                        })}
                        {move || alert.get().map(|alert| view! {
                            <div class="now-alert">
                                <p><strong>{alert.title}</strong> " " <span class="dim">{alert.reason}</span></p>
                                {move || if submitted.get() {
                                    view! { <p class="small dim" role="status">"Feedback saved."</p> }.into_any()
                                } else {
                                    view! {
                                        <div class="cluster" role="group" aria-label="Was this alert useful?">
                                            <span class="small dim">"Was this alert right?"</span>
                                            <Chip pressed=false on_click=Callback::new(move |()| submit(FeedbackVerdict::Useful))>"Useful"</Chip>
                                            <Chip pressed=false on_click=Callback::new(move |()| submit(FeedbackVerdict::NotUseful))>"Not useful"</Chip>
                                            <Chip pressed=false on_click=Callback::new(move |()| submit(FeedbackVerdict::FalsePositive))>"False positive"</Chip>
                                        </div>
                                    }.into_any()
                                }}
                            </div>
                        })}
                        {move || summary.get().filter(|s| !s.transcript_excerpt.is_empty()).map(|s| view! {
                            <Disclosure summary=format!("Transcript excerpt · {} s", number_string(js_round(s.window_seconds))) flush=true>
                                <p class="prose small transcript-excerpt">{s.transcript_excerpt}</p>
                                {s.speakers.map(|speakers| view! { <p class="small dim num">{speakers}</p> })}
                            </Disclosure>
                        })}
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}

fn kv_num(value: Option<f64>) -> String {
    value
        .map(|v| locale_number(js_round(v)))
        .unwrap_or_else(|| "—".to_owned())
}

#[component]
fn BaselinePanel(
    #[prop(into)] intelligence: Signal<Option<LivestreamIntelligence>>,
) -> impl IntoView {
    move || {
        intelligence.get().and_then(|i| i.trend).map(|t| view! {
            <Panel title="Baseline" pad=true>
                <dl class="kv">
                    <dt>"Current"</dt><dd class="num">{kv_num(t.current_viewers)}</dd>
                    <dt>"Baseline 5–20 min"</dt><dd class="num">{kv_num(t.baseline_viewers)}</dd>
                    <dt>"Change"</dt><dd><Delta percent=Some(t.percent_change) surge=t.anomalous/></dd>
                    <dt>"Slope"</dt><dd class="num">{format!("{}/min", number_string(js_round(t.viewers_per_minute)))}</dd>
                    <dt>"DGG now / base"</dt><dd class="num">{format!("{} / {}", kv_num(t.current_dgg_viewers), kv_num(t.baseline_dgg_viewers))}</dd>
                    <dt>"Samples"</dt><dd class="num">{kv_num(t.baseline_samples)}</dd>
                    <dt>"Surge gate"</dt><dd class="num">{kv_num(t.typical_peak_viewers)}</dd>
                    {t.suppression_reason.map(|r| view! { <dt>"Suppressed"</dt><dd class="text-warn">{r}</dd> })}
                    {t.reason.map(|r| view! { <dt>"Reason"</dt><dd>{r}</dd> })}
                    <dt>"Updated"</dt><dd class="num">{format_relative(t.updated_at as f64)}</dd>
                </dl>
            </Panel>
        })
    }
}

/// "Open destiny on Kick".
fn open_binding_label(account: &str, platform: &str) -> String {
    format!("Open {account} on {}", platform_label(platform))
}

/// "Open destiny on Kick, primary, 1,234 watching".
fn binding_row_label(
    account: &str,
    platform: &str,
    is_primary: bool,
    viewers: Option<i64>,
) -> String {
    let mut label = open_binding_label(account, platform);
    if is_primary {
        label.push_str(", primary");
    }
    if let Some(v) = viewers {
        label.push_str(&format!(", {} watching", locale_number(v as f64)));
    }
    label
}

#[component]
fn BindingsPanel(#[prop(into)] streamer: Signal<StreamerView>) -> impl IntoView {
    view! {
        <Panel title="Bindings">
            <div class="rows">
                {move || {
                    let s = streamer.get();
                    let (bindings, sources, primary) = match &s {
                        StreamerView::Live(l) => (l.bindings.clone(), l.sources.clone(), Some(l.primary.clone())),
                        StreamerView::Offline(o) => (o.bindings.clone(), Vec::new(), None),
                    };
                    bindings.into_iter().map(|b| {
                        let viewers = sources.iter().find(|src| src.platform == b.platform && src.username == b.username).and_then(|src| src.viewer_count);
                        let is_primary = primary.as_ref().is_some_and(|p| p.platform == b.platform && p.username == b.username);
                        view! {
                            <a
                                class="row dense"
                                href=b.url.clone()
                                target="_blank"
                                rel="noopener"
                                aria-label=binding_row_label(&b.username, &b.platform, is_primary, viewers)
                            >
                                <PlatformIcon platform=b.platform.clone()/>
                                <span class="row-main">
                                    <span class="row-title">{b.username.clone()}</span>
                                    <span class="row-sub">{platform_label(&b.platform)}</span>
                                </span>
                                <span class="row-end">
                                    {is_primary.then(|| view! { <Tag tone=Tone::Signal>"primary"</Tag> })}
                                    {viewers.map(|v| view! { <span class="num">{locale_number(v as f64)}</span> })}
                                </span>
                            </a>
                        }
                    }).collect_view()
                }}
            </div>
        </Panel>
    }
}

#[component]
fn RecordsPanel(
    metrics: StreamerMetricsResponse,
    sessions: Vec<StreamSessionView>,
) -> impl IntoView {
    let longest = sessions.iter().max_by_key(|s| s.duration_ms).cloned();
    view! {
        <Panel title="Records" pad=true>
            <dl class="kv">
                <dt>"All-time peak"</dt>
                <dd class="num">
                    {if metrics.all_time_max > 0 { locale_number(metrics.all_time_max as f64) } else { "—".to_owned() }}
                    {(metrics.all_time_max_timestamp != 0).then(|| view! { <span class="dim">{format!(" · {}", format_date_only(metrics.all_time_max_timestamp as f64))}</span> })}
                </dd>
                {longest.map(|l| view! {
                    <dt>"Longest stream"</dt>
                    <dd class="num">{format_duration(l.duration_ms as f64)} <span class="dim">{format!(" · {}", format_date_only(l.started_at as f64))}</span></dd>
                })}
                <dt>"Streams tracked"</dt><dd class="num">{sessions.len()}</dd>
                {metrics.platforms.into_iter().filter(|p| p.all_time_max > 0).map(|p| view! {
                    <dt>{format!("{} record", platform_label(&p.platform))}</dt>
                    <dd class="num">{locale_number(p.all_time_max as f64)}</dd>
                }).collect_view()}
            </dl>
        </Panel>
    }
}

fn display_name(streamer: &StreamerView) -> String {
    match streamer {
        StreamerView::Live(l) => l.display_name.clone(),
        StreamerView::Offline(o) => o.display_name.clone(),
    }
}

fn days_streamed_since(metrics: &StreamerMetricsResponse, days: i64) -> usize {
    let cutoff = days_ago(days);
    metrics
        .daily_buckets
        .iter()
        .filter(|b| b.date >= cutoff)
        .count()
}

#[component]
pub fn StreamerPage(#[prop(into)] streamer_id: String) -> impl IntoView {
    let live = use_live_data();
    let now = use_now(1000);
    let metrics = RwSignal::new(None::<StreamerMetricsResponse>);
    let error = RwSignal::new(None::<String>);
    let sessions = RwSignal::new(None::<Vec<StreamSessionView>>);
    let id = streamer_id.clone();
    let streamer = Memo::new(move |_| {
        live.snapshot.with(|s| {
            s.as_ref()
                .and_then(|s| s.streamers.iter().find(|v| v.id() == id).cloned())
        })
    });
    Effect::new(move |_| {
        if let Some(s) = streamer.get() {
            document().set_title(&format!("{} · Omni", display_name(&s)));
        }
    });
    use_page_label(move || streamer.with(|s| s.as_ref().map(display_name)));

    let metrics_id = streamer_id.clone();
    let load_metrics = move || {
        let id = metrics_id.clone();
        error.set(None);
        spawn_scoped(async move {
            match api::fetch_streamer_metrics(&id).await {
                Ok(m) => metrics.set(Some(m)),
                Err(e) => error.set(Some(e.message().to_owned())),
            }
        });
    };
    load_metrics();
    let sessions_id = streamer_id.clone();
    spawn_scoped(async move {
        // Session history is supplementary; the page stays useful without it.
        sessions.set(Some(
            api::fetch_streamer_sessions(&sessions_id)
                .await
                .map(|r| r.sessions)
                .unwrap_or_default(),
        ));
    });

    let live_view = Memo::new(move |_| match streamer.get() {
        Some(StreamerView::Live(l)) => Some(l),
        _ => None,
    });
    let intelligence =
        Memo::new(move |_| live_view.with(|l| l.as_ref().and_then(streamer_intelligence)));
    // The page layout depends only on live-ness; reading `live_view` in the
    // page closure would rebuild it (resetting the chart range, expanded
    // weeks and disclosures) on every snapshot.
    let is_live = Memo::new(move |_| live_view.with(Option::is_some));
    let read = Memo::new(move |_| {
        let now = now.get();
        intelligence.with(|i| i.as_ref().and_then(|i| live_read(i, now)))
    });
    let typical = Memo::new(move |_| {
        intelligence
            .with(|i| {
                i.as_ref()
                    .and_then(|i| i.trend.as_ref()?.typical_peak_viewers)
            })
            .or_else(|| sessions.with(|s| s.as_deref().and_then(typical_peak)))
    });
    let history_id = streamer_id.clone();
    let points = Signal::derive(move || {
        live.viewer_history.with(|h| {
            h.get(&history_id)
                .map(|v| v.iter().map(|(_, n)| *n).collect())
                .unwrap_or_default()
        })
    });
    let page_id = streamer_id.clone();
    let retry = Callback::new(move |()| load_metrics());

    move || {
        let Some(initial) = streamer.get_untracked().or_else(|| streamer.get()) else {
            if live.snapshot.with(Option::is_none) {
                return view! { <Skeleton kind=SkeletonKind::Title width="30%"/> <SkeletonRows count=6/> }.into_any();
            }
            return view! {
                <ErrorState
                    title="Unknown streamer"
                    detail=format!("No channel named \u{201c}{page_id}\u{201d} is being monitored.")
                    link=("All streamers".to_owned(), "/live".to_owned())
                    page=true
                />
            }
            .into_any();
        };
        let is_live = is_live.get();
        let current = Signal::derive(move || streamer.get().unwrap_or_else(|| initial.clone()));
        let name = current.with_untracked(display_name);
        let (bindings, tier) = current.with_untracked(|s| match s {
            StreamerView::Live(l) => (l.bindings.clone(), l.tier),
            StreamerView::Offline(o) => (o.bindings.clone(), o.tier),
        });
        let primary = current.with_untracked(|s| match s {
            StreamerView::Live(l) => Some(l.primary.clone()),
            StreamerView::Offline(o) => o.bindings.first().cloned(),
        });
        let avatar = current.with_untracked(|s| s.avatar_url().map(str::to_owned));
        let intel_href = format!("/streamers/{}/intelligence", encode_uri_component(&page_id));
        let sub_line = move || match current.get() {
            StreamerView::Live(l) => clean_stream_title(&l.title),
            StreamerView::Offline(o) => match o.last_ended_at {
                Some(ended) => format!(
                    "Last live {}{}",
                    format_relative(ended as f64),
                    o.last_max_viewer_count
                        .filter(|p| *p > 0)
                        .map(|p| format!(" · peak {}", locale_number(p as f64)))
                        .unwrap_or_default()
                ),
                None => "No streams seen yet".to_owned(),
            },
        };
        let watch = primary.clone().map(|p| {
            let label = if is_live { format!("Watch on {}", platform_label(&p.platform)) } else { "Open channel".to_owned() };
            let aria = if is_live { watch_label(&name, Some(&p)) } else { open_binding_label(&name, &p.platform) };
            view! { <ButtonLink to=p.url variant=ButtonVariant::Primary external=true title=aria.clone() aria_label=aria>{label}</ButtonLink> }
        });
        view! {
            <header class="page-head streamer-hero">
                <Avatar name=name.clone() size=64 presence=if is_live { Presence::Live } else { Presence::Offline } platform=primary.as_ref().map(|p| p.platform.clone()).unwrap_or_default() image=avatar/>
                <div class="page-head-text">
                    <div class="cluster">
                        {move || live_view.get().map(|l| view! { <LiveTag detail=format_uptime(now.get() - l.started_at as f64)/> })}
                        {(tier == StreamerTier::Background).then(|| view! {
                            <Tag title="Background tier: live, offline and title notifications are muted">"Background tier"</Tag>
                        })}
                    </div>
                    <h1 class="sentence">{name.clone()}</h1>
                    {move || match read.get() {
                        Some(r) => view! {
                            <p class="hero-topic">{r.topic}</p>
                            <p class="lede">{r.summary}</p>
                            <p class="stream-title" title="Stream title">{sub_line()}</p>
                        }
                        .into_any(),
                        None => view! { <p class="lede">{sub_line()}</p> }.into_any(),
                    }}
                    <div class="chips">
                        {bindings.into_iter().map(|b| view! {
                            <a
                                class="chip"
                                href=b.url.clone()
                                target="_blank"
                                rel="noopener"
                                aria-label=open_binding_label(&b.username, &b.platform)
                                title=open_binding_label(&b.username, &b.platform)
                            >
                                <PlatformIcon platform=b.platform.clone() size=14/>
                                {b.username.clone()}
                            </a>
                        }).collect_view()}
                        {move || current.with(|s| match s {
                            StreamerView::Live(l) => l.dgg,
                            StreamerView::Offline(o) => o.dgg,
                        }).map(|d| view! { <omni_web_kit::components::DggPresenceTag dgg=Some(d)/> })}
                    </div>
                </div>
                <div class="page-actions">
                    <ButtonLink to=intel_href icon=Icon::Pulse>"Intelligence"</ButtonLink>
                    {watch}
                </div>
            </header>

            <Panel stage=is_live aria_label="Viewers" class="readouts streamer-readouts">
                {if is_live {
                    view! {
                        <Readout
                            label="Watching now"
                            value=Signal::derive(move || live_view.with(|l| l.as_ref().map(|l| locale_number(viewer_number(l) as f64)).unwrap_or_default()))
                            size=ReadoutSize::Xl
                            tone=Tone::Signal
                        >
                            <Sparkline points reference=typical tone=Tone::Signal height=40 label="Viewers this session"/>
                        </Readout>
                        <Readout
                            label="Session peak"
                            value=Signal::derive(move || live_view.with(|l| l.as_ref().map(|l| locale_number(l.max_viewer_count as f64)).unwrap_or_default()))
                        >
                            {move || live_view.with(|l| l.as_ref().map(|l| format!("since {}", format_session_time(l.started_at))))}
                        </Readout>
                        <Readout
                            label="DGG"
                            value=Signal::derive(move || live_view.with(|l| l.as_ref().and_then(|l| l.dgg).map(|d| if d.hosted { "Hosted".to_owned() } else { d.viewers.map(|v| locale_number(v as f64)).unwrap_or_else(|| "Embedded".to_owned()) })).unwrap_or_else(|| "—".to_owned()))
                        >
                            <Delta percent=Signal::derive(move || intelligence.with(|i| i.as_ref().and_then(|i| i.trend.as_ref()?.dgg_percent_change)))/>
                        </Readout>
                        <Readout label="Typical peak" value=Signal::derive(move || typical.get().map(|t| locale_number(js_round(t))).unwrap_or_else(|| "—".to_owned()))>
                            {move || metrics.with(|m| m.as_ref().filter(|m| m.all_time_max > 0).map(|m| format!("record {} · {}", format_compact_number(m.all_time_max as f64), format_date_only(m.all_time_max_timestamp as f64))))}
                        </Readout>
                    }
                    .into_any()
                } else {
                    view! {
                        <Readout label="Last peak" value=Signal::derive(move || current.with(|s| match s {
                            StreamerView::Offline(o) => o.last_max_viewer_count.map(|p| locale_number(p as f64)),
                            StreamerView::Live(_) => None,
                        }).unwrap_or_else(|| "—".to_owned()))/>
                        <Readout label="Typical peak" value=Signal::derive(move || typical.get().map(|t| locale_number(js_round(t))).unwrap_or_else(|| "—".to_owned()))/>
                        <Readout label="All-time" value=Signal::derive(move || metrics.with(|m| m.as_ref().filter(|m| m.all_time_max > 0).map(|m| locale_number(m.all_time_max as f64))).unwrap_or_else(|| "—".to_owned()))>
                            {move || metrics.with(|m| m.as_ref().filter(|m| m.all_time_max_timestamp != 0).map(|m| format_date_only(m.all_time_max_timestamp as f64)))}
                        </Readout>
                        <Readout label="Streams in 30 days" value=Signal::derive(move || metrics.with(|m| m.as_ref().map(|m| days_streamed_since(m, 30).to_string())).unwrap_or_else(|| "—".to_owned()))/>
                    }
                    .into_any()
                }}
            </Panel>

            <div class="split">
                <div class="stack-lg">
                    {move || match (metrics.get(), error.get()) {
                        (Some(m), _) if !m.daily_buckets.is_empty() || m.all_time_max > 0 => view! { <ViewerChart metrics=m typical/> }.into_any(),
                        (Some(_), _) => view! { <Panel title="Daily peak viewers" pad=true><EmptyState compact=true message="No viewer data recorded yet."/></Panel> }.into_any(),
                        (None, Some(e)) => view! { <ErrorState title="Viewer metrics could not load" raw=e retry/> }.into_any(),
                        (None, None) => view! { <Panel title="Daily peak viewers" pad=true><div class="chart-container"><Skeleton width="100%"/></div></Panel> }.into_any(),
                    }}
                    {move || match sessions.get() {
                        None => view! { <SkeletonRows count=5 label="Loading streams"/> }.into_any(),
                        Some(list) if list.is_empty() && !is_live => view! { <EmptyState compact=true message="No completed streams yet."/> }.into_any(),
                        Some(list) => {
                            let record = metrics.with(|m| m.as_ref().map_or(0, |m| m.all_time_max));
                            view! { <Streams sessions=list record typical=typical.get_untracked() live=live_view/> }.into_any()
                        }
                    }}
                </div>
                <div class="sticky-side">
                    <NowPanel streamer_id=page_id.clone() intelligence is_live/>
                    {is_live.then(|| view! { <BaselinePanel intelligence/> })}
                    <BindingsPanel streamer=current/>
                    {move || metrics.get().map(|m| view! { <RecordsPanel metrics=m sessions=sessions.get().unwrap_or_default()/> })}
                </div>
            </div>
        }
        .into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_links_name_account_and_platform() {
        assert_eq!(
            open_binding_label("destiny", "kick"),
            "Open destiny on Kick"
        );
        assert_eq!(
            binding_row_label("destiny", "youtube", true, Some(1234)),
            "Open destiny on YouTube, primary, 1,234 watching"
        );
        assert_eq!(
            binding_row_label("destiny", "twitch", false, None),
            "Open destiny on Twitch"
        );
    }

    fn session(started: i64, peak: i64) -> StreamSessionView {
        StreamSessionView {
            started_at: started,
            ended_at: started + 3_600_000,
            duration_ms: 3_600_000,
            peak_viewers: peak,
            title: String::new(),
            platform: "twitch".into(),
            username: "x".into(),
        }
    }

    #[test]
    fn typical_peak_is_the_recent_median() {
        let list = [
            session(0, 100),
            session(0, 300),
            session(0, 200),
            session(0, 0),
        ];
        assert_eq!(typical_peak(&list), Some(200.0));
        assert_eq!(typical_peak(&[]), None);
    }

    #[test]
    fn sessions_group_by_week() {
        let day = 86_400_000;
        let monday = days_from_civil(2026, 10, 5);
        let list = [
            session((monday + 8) * day, 1),
            session((monday + 7) * day, 1),
            session((monday + 6) * day, 1),
            session(monday * day, 1),
        ];
        let weeks = group_by_week(&list, |ms| ms.div_euclid(day));
        let sizes: Vec<usize> = weeks.iter().map(|w| w.sessions.len()).collect();
        assert_eq!(sizes, [2, 2]);
    }
}
