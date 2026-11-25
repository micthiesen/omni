//! MCP tool-call activity: visibility-aware 10 s polling, a tools table,
//! status/tool filters, "Load older" paging and a call inspector.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use leptos::prelude::*;
use omni_api::mcp_activity::{
    McpActivityResponse, McpActivitySummary, McpCall, McpCallStatus, McpToolSummary,
};
use omni_web_kit::api::{self, ApiClientError};
use omni_web_kit::components::mcp_badges::{call_status_kind, policy_str};
use omni_web_kit::components::{
    Button, ButtonLink, ButtonSize, ButtonVariant, CallStatusPill, CellKind, EmptyState,
    ErrorState, Icon, InlineNote, Inspector, PageHead, Panel, PolicyBadge, Readout, ReadoutBand,
    RunCell, RunStrip, SegOption, Segmented, SkeletonRows, Status, StatusKind, Tone,
};
use omni_web_kit::hooks::{use_is_wide, use_now, use_visible_poll};
use omni_web_kit::router::Link;
use omni_web_kit::task::{TaskHandle, spawn_detached};
use omni_web_kit::utils::claude_activity::{format_json, is_claude_tool};
use omni_web_kit::utils::format::{format_absolute, format_duration, format_relative_at};
use omni_web_kit::utils::js::{js_round, locale_number, number_string};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StatusFilter {
    All,
    Error,
    Running,
}

impl StatusFilter {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Error => "error",
            Self::Running => "running",
        }
    }

    fn status(self) -> Option<McpCallStatus> {
        match self {
            Self::All => None,
            Self::Error => Some(McpCallStatus::Error),
            Self::Running => Some(McpCallStatus::Running),
        }
    }
}

const STATUS_FILTERS: [(StatusFilter, &str); 3] = [
    (StatusFilter::All, "All"),
    (StatusFilter::Error, "Errors"),
    (StatusFilter::Running, "Running"),
];
const STRIP: usize = 12;

/// The error's message, else the fallback.
pub(crate) fn error_message(message: &str, fallback: &str) -> String {
    if message.is_empty() {
        fallback.to_owned()
    } else {
        message.to_owned()
    }
}

/// Merges the polled first page with older pages, dropping duplicates.
pub(crate) fn merge_calls(head: &[McpCall], older: &[McpCall]) -> Vec<McpCall> {
    let seen: HashSet<&str> = head.iter().map(|c| c.call_id.as_str()).collect();
    head.iter()
        .chain(older.iter().filter(|c| !seen.contains(c.call_id.as_str())))
        .cloned()
        .collect()
}

fn cell_kind(status: McpCallStatus) -> CellKind {
    match status {
        McpCallStatus::Ok => CellKind::Ok,
        McpCallStatus::Error => CellKind::Fault,
        McpCallStatus::Running => CellKind::Running,
        McpCallStatus::Interrupted => CellKind::Warn,
        McpCallStatus::Invalid => CellKind::Warn,
    }
}

/// The newest `slots` outcomes per tool from newest-first calls, oldest first.
pub(crate) fn recent_cells(calls: &[McpCall], slots: usize) -> HashMap<String, Vec<RunCell>> {
    let mut by_tool: HashMap<String, Vec<RunCell>> = HashMap::new();
    for call in calls {
        let cells = by_tool.entry(call.tool.clone()).or_default();
        if cells.len() < slots {
            let duration = call
                .duration_ms
                .map(|d| format!(" · {}", format_duration(d as f64)))
                .unwrap_or_default();
            cells.push(RunCell {
                kind: cell_kind(call.status),
                title: format!("{}{duration}", format_absolute(call.started_at as f64)),
            });
        }
    }
    for cells in by_tool.values_mut() {
        cells.reverse();
    }
    by_tool
}

fn duration_text(call: &McpCall, now: f64) -> String {
    match call.duration_ms {
        Some(ms) => format_duration(ms as f64),
        None if call.status == McpCallStatus::Running => {
            format_duration(now - call.started_at as f64)
        }
        None => "—".to_owned(),
    }
}

#[component]
fn ToolsTable(
    tools: Memo<Vec<McpToolSummary>>,
    strips: RwSignal<HashMap<String, Vec<RunCell>>>,
    active_tool: RwSignal<String>,
    now: ReadSignal<f64>,
) -> impl IntoView {
    let row = move |tool: McpToolSummary| {
        let name = tool.tool.clone();
        let selected = {
            let name = name.clone();
            Memo::new(move |_| active_tool.with(|t| *t == name))
        };
        let toggle = {
            let name = name.clone();
            move || {
                active_tool.set(if selected.get_untracked() {
                    String::new()
                } else {
                    name.clone()
                })
            }
        };
        let toggle_key = toggle.clone();
        let rate = if tool.calls > 0 {
            tool.errors as f64 / tool.calls as f64
        } else {
            0.0
        };
        let last_at = tool.last_at as f64;
        let strip_name = name.clone();
        let cells = Signal::derive(move || {
            strips.with(|s| s.get(&strip_name).cloned().unwrap_or_default())
        });
        view! {
            <tr
                data-row="true"
                tabindex="0"
                class=move || if selected.get() { "clickable selected" } else { "clickable" }
                aria-selected=move || selected.get().to_string()
                title="Show only this tool's calls"
                on:click=move |_| toggle()
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        toggle_key();
                    }
                }
            >
                <td class="grow">
                    <div class="cell-two">
                        <span class="truncate strong">{tool.title.clone()}</span>
                        <span class="truncate mono small muted">{tool.tool.clone()}</span>
                    </div>
                </td>
                <td class="numeric">{locale_number(tool.calls as f64)}</td>
                <td class={if tool.errors > 0 { "numeric text-fault" } else { "numeric muted" }}>
                    {locale_number(tool.errors as f64)}
                    {(tool.errors > 0).then(|| view! { <span class="small">{format!(" {}%", number_string(js_round(rate * 100.0)))}</span> })}
                </td>
                <td class="numeric hide-below-desk">
                    {tool.avg_duration_ms.map_or_else(|| "—".to_owned(), |avg| format_duration(avg as f64))}
                </td>
                <td class="hide-below-wide"><PolicyBadge policy=policy_str(tool.recommended_policy) /></td>
                <td class="hide-below-wide">
                    <RunStrip cells slots=STRIP label=format!("Recent {} outcomes", tool.title) />
                </td>
                <td class="numeric nowrap hide-below-desk" title=format_absolute(last_at)>
                    {move || format_relative_at(last_at, now.get())}
                </td>
            </tr>
        }
    };
    view! {
        <div class="table-wrap">
            <table class="table dense mcp-tools" data-primary-rows="true">
                <thead>
                    <tr>
                        <th>"Tool"</th>
                        <th class="numeric">"Calls"</th>
                        <th class="numeric">"Errors"</th>
                        <th class="numeric hide-below-desk">"Avg"</th>
                        <th class="hide-below-wide">"Policy"</th>
                        <th class="hide-below-wide">"Recent"</th>
                        <th class="numeric hide-below-desk">"Last used"</th>
                    </tr>
                </thead>
                <tbody>
                    <For each=move || tools.get() key=|t| format!("{t:?}") children=row />
                </tbody>
            </table>
        </div>
    }
}

#[component]
fn CallInspector(
    call: Signal<McpCall>,
    docked: Signal<bool>,
    on_close: Callback<()>,
) -> impl IntoView {
    let title = Signal::derive(move || call.with(|c| c.title.clone()));
    let now = use_now(1000);
    view! {
        <Inspector
            title
            docked
            on_close
            status=ViewFn::from(move || {
                let status = call.with(|c| c.status);
                view! { <CallStatusPill status /> }
            })
        >
            {move || {
                let c = call.get();
                let claude = is_claude_tool(&c.tool);
                let started = c.started_at as f64;
                let running_call = c.clone();
                view! {
                    <section class="inspector-section">
                        <dl class="kv">
                            <dt>"Tool"</dt>
                            <dd class="mono">{c.tool.clone()}</dd>
                            {c.caller.clone().map(|caller| view! { <dt>"Caller"</dt><dd>{caller}</dd> })}
                            {c.integration.clone().map(|id| view! { <dt>"Integration"</dt><dd class="mono">{id}</dd> })}
                            <dt>"Started"</dt>
                            <dd class="num">{format_absolute(started)}</dd>
                            {c.finished_at.map(|f| view! { <dt>"Finished"</dt><dd class="num">{format_absolute(f as f64)}</dd> })}
                            <dt>"Duration"</dt>
                            <dd class="num">{move || duration_text(&running_call, now.get())}</dd>
                            <dt>"Access"</dt>
                            <dd>{if c.read_only { "Read-only" } else { "Writes" }}</dd>
                            <dt>"Policy"</dt>
                            <dd><PolicyBadge policy=policy_str(c.recommended_policy) /></dd>
                            <dt>"Call id"</dt>
                            <dd class="mono small">{c.call_id.clone()}</dd>
                        </dl>
                        {claude.then(|| view! { <Link to="/claude" class="textlink small">"View in Claude Code"</Link> })}
                    </section>
                    {c.error.clone().filter(|e| !e.is_empty()).map(|e| view! {
                        <section class="inspector-section">
                            <h3>"Error"</h3>
                            <pre class="json-block mcp-error">{e}</pre>
                        </section>
                    })}
                    <section class="inspector-section">
                        <h3>"Input"</h3>
                        <pre class="json-block">{format_json(&c.input)}</pre>
                    </section>
                    {(!c.output.is_null()).then(|| view! {
                        <section class="inspector-section">
                            <h3>"Output"</h3>
                            <pre class="json-block">{format_json(&c.output)}</pre>
                        </section>
                    })}
                    {(!c.log.is_empty()).then(|| {
                        let lines: String = c
                            .log
                            .iter()
                            .map(|line| format!("{} {:5} {}: {}", format_absolute(line.at as f64), line.level, line.target, line.message))
                            .collect::<Vec<_>>()
                            .join("\n");
                        view! {
                            <section class="inspector-section">
                                <h3>"Log"</h3>
                                <pre class="json-block">{lines}</pre>
                            </section>
                        }
                    })}
                }
            }}
        </Inspector>
    }
}

#[component]
pub fn McpPage() -> impl IntoView {
    let now = use_now(15_000);
    let wide = use_is_wide();
    let status_filter = RwSignal::new(StatusFilter::All);
    let tool = RwSignal::new(String::new());
    let head = RwSignal::new(None::<McpActivityResponse>);
    let error = RwSignal::new(None::<String>);
    let older = RwSignal::new((Vec::<McpCall>::new(), None::<i64>));
    let head_key = RwSignal::new(String::new());
    let loading_older = RwSignal::new(false);
    let older_error = RwSignal::new(None::<String>);
    let selected = RwSignal::new(None::<McpCall>);
    let strips = RwSignal::new(HashMap::<String, Vec<RunCell>>::new());
    let cancel_older = StoredValue::new(None::<TaskHandle>);

    let filter_key = Memo::new(move |_| format!("{}:{}", status_filter.get().as_str(), tool.get()));
    let filter_status = move || status_filter.get_untracked().status();
    let filter_tool = move || Some(tool.get_untracked()).filter(|t| !t.is_empty());

    let reload = use_visible_poll(
        filter_key.into(),
        move || {
            let tool = filter_tool();
            let status = filter_status();
            let key = filter_key.get_untracked();
            async move {
                api::fetch_mcp_activity(Some(100), tool.as_deref(), status, None)
                    .await
                    .map(|response| (key, response))
            }
        },
        move |(key, response): (String, McpActivityResponse)| {
            // Recent outcomes per tool come only from the unfiltered page.
            if key == "all:" {
                strips.set(recent_cells(&response.calls, STRIP));
            }
            head.set(Some(response));
            head_key.set(key);
            error.set(None);
        },
        move |err: ApiClientError| {
            error.set(Some(error_message(
                err.message(),
                "Failed to load MCP activity",
            )));
        },
        Duration::from_secs(10),
    );

    // A filter change discards older pages loaded under the previous filter
    // and abandons an in-flight "Load older" request.
    Effect::new(move |_| {
        filter_key.track();
        older.set((Vec::new(), None));
        older_error.set(None);
        loading_older.set(false);
        if let Some(Some(handle)) = cancel_older.try_get_value() {
            handle.abort();
        }
    });

    // Stats and tool summaries stay visible while a new filter loads its calls.
    let calls_current =
        Memo::new(move |_| head.with(Option::is_some) && head_key.with(|k| *k == filter_key.get()));
    let calls = Memo::new(move |_| {
        if !calls_current.get() {
            return Vec::new();
        }
        head.with(|h| {
            h.as_ref()
                .map(|h| older.with(|(o, _)| merge_calls(&h.calls, o)))
                .unwrap_or_default()
        })
    });
    let next_before = Memo::new(move |_| {
        if !calls_current.get() {
            return None;
        }
        older.with(|(calls, next)| {
            if calls.is_empty() {
                head.with(|h| h.as_ref().and_then(|h| h.next_before))
            } else {
                *next
            }
        })
    });

    let load_older = move |_| {
        let Some(before) = next_before.get_untracked() else {
            return;
        };
        if loading_older.get_untracked() {
            return;
        }
        loading_older.set(true);
        older_error.set(None);
        let tool = filter_tool();
        let status = filter_status();
        let handle = spawn_detached(async move {
            match api::fetch_mcp_activity(Some(100), tool.as_deref(), status, Some(before)).await {
                Ok(response) => {
                    older.update(|(calls, next)| {
                        calls.extend(response.calls);
                        *next = response.next_before;
                    });
                    loading_older.set(false);
                }
                Err(err) => {
                    older_error.set(Some(error_message(
                        err.message(),
                        "Failed to load older calls",
                    )));
                    loading_older.set(false);
                }
            }
        });
        cancel_older.set_value(Some(handle));
    };
    on_cleanup(move || {
        if let Some(Some(handle)) = cancel_older.try_get_value() {
            handle.abort();
        }
    });

    let summary = Memo::new(move |_| head.with(|h| h.as_ref().map(|h| h.summary)));
    let tools =
        Memo::new(move |_| head.with(|h| h.as_ref().map(|h| h.tools.clone()).unwrap_or_default()));
    let max_calls =
        Memo::new(move |_| head.with(|h| h.as_ref().map_or(0, |h| h.retention.max_calls)));
    // Only the names feed the select: polled counts must not rebuild it (an
    // open dropdown would close and focus would drop).
    let tool_options = Memo::new(move |_| {
        tools.with(|t| {
            t.iter()
                .map(|o| (o.tool.clone(), o.title.clone()))
                .collect::<Vec<_>>()
        })
    });
    let tools_empty = Memo::new(move |_| tools.with(Vec::is_empty));
    let calls_empty = Memo::new(move |_| calls.with(Vec::is_empty));

    let tool_select = move || {
        let current = tool.get();
        let options = tool_options.get();
        let extra = (!current.is_empty() && !options.iter().any(|(name, _)| *name == current))
            .then(|| {
                let value = current.clone();
                view! { <option value=value.clone() prop:selected=true>{value.clone()}</option> }
            });
        view! {
            <select
                class="select mcp-tool-select"
                aria-label="Filter by tool"
                on:change=move |event| tool.set(event_target_value(&event))
            >
                <option value="" prop:selected=current.is_empty()>"All tools"</option>
                {extra}
                {options
                    .into_iter()
                    .map(|(name, title)| {
                        let selected = name == current;
                        view! {
                            <option value=name prop:selected=selected>
                                {title}
                            </option>
                        }
                    })
                    .collect_view()}
            </select>
        }
    };

    let stat = move |pick: fn(&McpActivitySummary) -> u64| {
        Signal::derive(move || summary.get().map_or(0, |s| pick(&s)))
    };
    let (last24h, stored, errors, approvals, running) = (
        stat(|s| s.last24h),
        stat(|s| s.stored),
        stat(|s| s.errors24h),
        stat(|s| s.approval_calls24h),
        stat(|s| s.running),
    );
    let readouts = move || {
        view! {
            <ReadoutBand cols=4 aria_label="Last 24 hours">
                <Readout label="Calls · 24h" value=Signal::derive(move || locale_number(last24h.get() as f64))>
                    {move || format!("{} stored", locale_number(stored.get() as f64))}
                </Readout>
                <Readout
                    label="Errors · 24h"
                    value=Signal::derive(move || locale_number(errors.get() as f64))
                    tone=Signal::derive(move || if errors.get() > 0 { Tone::Fault } else { Tone::Neutral })
                />
                <Readout label="Approvals · 24h" value=Signal::derive(move || locale_number(approvals.get() as f64))>
                    "Calls under an approval policy"
                </Readout>
                <Readout
                    label="Running"
                    value=Signal::derive(move || locale_number(running.get() as f64))
                    tone=Signal::derive(move || if running.get() > 0 { Tone::Signal } else { Tone::Neutral })
                />
            </ReadoutBand>
        }
    };

    let call_row = move |call: McpCall| {
        let id = call.call_id.clone();
        let is_selected = {
            let id = id.clone();
            Memo::new(move |_| selected.with(|s| s.as_ref().is_some_and(|s| s.call_id == id)))
        };
        let open = {
            let call = call.clone();
            move || selected.set(Some(call.clone()))
        };
        let open_key = open.clone();
        let started = call.started_at as f64;
        let kind = call_status_kind(call.status);
        let duration_call = call.clone();
        let error_line = call.error.clone().filter(|e| !e.is_empty());
        view! {
            <tr
                data-row="true"
                tabindex="0"
                class=move || if is_selected.get() { "clickable selected" } else { "clickable" }
                aria-selected=move || is_selected.get().to_string()
                on:click=move |_| open()
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        open_key();
                    }
                }
            >
                <td class="cell-status">
                    <Status kind=kind label=omni_web_kit::components::mcp_badges::call_status_str(call.status).to_owned() dot_only=kind == StatusKind::Ok />
                </td>
                <td class="grow">
                    <div class="cell-two">
                        <span class="truncate strong">{call.title.clone()}</span>
                        {match error_line {
                            Some(e) => view! { <span class="truncate small text-fault">{e}</span> }.into_any(),
                            None => view! { <span class="truncate mono small muted">{call.tool.clone()}</span> }.into_any(),
                        }}
                    </div>
                </td>
                <td class="hide-below-wide"><PolicyBadge policy=policy_str(call.recommended_policy) /></td>
                <td class="numeric nowrap hide-below-desk">{move || duration_text(&duration_call, now.get())}</td>
                <td class="numeric nowrap" title=format_absolute(started)>
                    {move || format_relative_at(started, now.get())}
                </td>
            </tr>
        }
    };

    let status_options = Signal::stored(
        STATUS_FILTERS
            .iter()
            .map(|(value, label)| SegOption::new(*value, *label))
            .collect::<Vec<_>>(),
    );
    let calls_panel = move || {
        view! {
            <Panel
                title="Calls"
                class="mcp-calls"
                refreshing=Signal::derive(move || head.with(Option::is_some) && !calls_current.get())
                head_end=ViewFn::from(move || view! {
                    <span>{move || format!("Keeps the latest {}", locale_number(max_calls.get() as f64))}</span>
                })
            >
                <div class="toolbar mcp-toolbar">
                    <Segmented
                        options=status_options
                        value=status_filter
                        on_change=Callback::new(move |v| status_filter.set(v))
                        aria_label="Call status"
                        small=true
                    />
                    {tool_select}
                    {move || {
                        (!tool.with(String::is_empty))
                            .then(|| view! {
                                <Button variant=ButtonVariant::Ghost size=ButtonSize::Sm on_click=Callback::new(move |_| tool.set(String::new()))>
                                    "Clear tool"
                                </Button>
                            })
                    }}
                </div>
                {move || {
                    if !calls_current.get() && calls_empty.get() {
                        view! { <SkeletonRows count=8 label="Loading calls" /> }.into_any()
                    } else if calls_empty.get() {
                        view! {
                            <EmptyState
                                compact=true
                                message="No calls in this view."
                                action=ViewFn::from(move || view! {
                                    <Button size=ButtonSize::Sm on_click=Callback::new(move |_| {
                                        status_filter.set(StatusFilter::All);
                                        tool.set(String::new());
                                    })>
                                        "Clear filters"
                                    </Button>
                                })
                            />
                        }
                            .into_any()
                    } else {
                        view! {
                            <div class="table-wrap">
                                <table class="table dense mcp-call-table" data-primary-rows="true">
                                    <thead>
                                        <tr>
                                            <th class="cell-status"><span class="sr-only">"Status"</span></th>
                                            <th>"Call"</th>
                                            <th class="hide-below-wide">"Policy"</th>
                                            <th class="numeric hide-below-desk">"Duration"</th>
                                            <th class="numeric">"Started"</th>
                                        </tr>
                                    </thead>
                                    <tbody>
                                        <For
                                            each=move || calls.get()
                                            key=|call| format!("{}|{call:?}", call.call_id)
                                            children=call_row
                                        />
                                    </tbody>
                                </table>
                            </div>
                        }
                            .into_any()
                    }
                }}
                {move || older_error.get().map(|e| view! { <div class="panel-body"><ErrorState title="Older calls could not load" raw=e /></div> })}
                {move || {
                    next_before
                        .get()
                        .is_some()
                        .then(|| {
                            view! {
                                <div class="panel-foot mcp-older">
                                    <Button
                                        variant=ButtonVariant::Ghost
                                        size=ButtonSize::Sm
                                        busy=loading_older
                                        on_click=Callback::new(load_older)
                                    >
                                        "Load older calls"
                                    </Button>
                                </div>
                            }
                        })
                }}
            </Panel>
        }
    };

    // The inspector follows the polled copy of the selected call. A Memo, so
    // a poll that leaves the call unchanged does not rebuild the inspector.
    let live_selected = Memo::new(move |_| {
        let chosen = selected.get()?;
        Some(
            calls
                .with(|c| c.iter().find(|c| c.call_id == chosen.call_id).cloned())
                .unwrap_or(chosen),
        )
    });
    let has_selected = Memo::new(move |_| selected.with(Option::is_some));
    let inspector = move || {
        has_selected.get().then(|| {
            view! {
                <CallInspector
                    call=Signal::derive(move || live_selected.get().unwrap_or_else(blank_call))
                    docked=Signal::derive(move || wide.get())
                    on_close=Callback::new(move |()| selected.set(None))
                />
            }
        })
    };

    let has_head = Memo::new(move |_| head.with(Option::is_some));
    // Failed polls set the same message again; only a change may rebuild the
    // error state (its Details disclosure would collapse).
    let error_text = Memo::new(move |_| error.get());
    view! {
        <PageHead
            title="MCP activity"
            lede="Every tool call agents made through Omni's MCP server."
            actions=ViewFn::from(|| view! {
                <ButtonLink to="/claude" icon=Icon::Terminal>"Claude Code"</ButtonLink>
            })
        />
        {move || {
            if has_head.get() {
                return None;
            }
            Some(match error_text.get() {
                Some(e) => view! {
                    <ErrorState
                        title="MCP activity could not load"
                        raw=e
                        retry=Callback::new(move |()| reload.run(()))
                        page=true
                    />
                }
                .into_any(),
                None => view! { <SkeletonRows count=8 label="Loading MCP activity" /> }.into_any(),
            })
        }}
        {move || {
            has_head
                .get()
                .then(|| {
                    view! {
                        {move || {
                            error
                                .get()
                                .map(|e| view! {
                                    <InlineNote tone=Tone::Warn role="status">
                                        {format!("Refresh failed ({e}). Showing the last loaded state.")}
                                    </InlineNote>
                                })
                        }}
                        {readouts}
                        <div class=move || if selected.with(Option::is_some) && wide.get() { "split docked mcp-layout" } else { "mcp-layout" }>
                            <div class="stack-lg">
                                <Panel
                                    title="Tools"
                                    head_end=ViewFn::from(move || view! { <span class="num">{move || tools.with(Vec::len)}</span> })
                                >
                                    {move || {
                                        if tools_empty.get() {
                                            view! { <EmptyState compact=true message="No tools called yet." /> }.into_any()
                                        } else {
                                            view! { <ToolsTable tools strips active_tool=tool now /> }.into_any()
                                        }
                                    }}
                                </Panel>
                                {calls_panel}
                            </div>
                            {inspector}
                        </div>
                    }
                })
        }}
    }
}

/// Stand-in read once while the inspector unmounts; never shown.
fn blank_call() -> McpCall {
    McpCall {
        call_id: String::new(),
        tool: String::new(),
        title: String::new(),
        recommended_policy: omni_api::mcp_activity::RecommendedPolicy::Allow,
        read_only: true,
        started_at: 0,
        finished_at: None,
        duration_ms: None,
        status: McpCallStatus::Ok,
        error: None,
        input: serde_json::Value::Null,
        output: serde_json::Value::Null,
        caller: None,
        integration: None,
        log: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use omni_api::mcp_activity::{McpCall, McpCallStatus, RecommendedPolicy};
    use serde_json::Value;

    use super::{error_message, merge_calls, recent_cells};
    use omni_web_kit::components::CellKind;

    fn call(id: &str) -> McpCall {
        McpCall {
            call_id: id.into(),
            tool: "t".into(),
            title: "T".into(),
            recommended_policy: RecommendedPolicy::Allow,
            read_only: true,
            started_at: 1,
            finished_at: None,
            duration_ms: None,
            status: McpCallStatus::Ok,
            error: None,
            input: Value::Null,
            output: Value::Null,
            caller: None,
            integration: None,
            log: Vec::new(),
        }
    }

    #[test]
    fn older_pages_append_without_duplicates() {
        let merged = merge_calls(&[call("a"), call("b")], &[call("b"), call("c")]);
        let ids: Vec<&str> = merged.iter().map(|c| c.call_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
    }

    #[test]
    fn recent_cells_keep_the_newest_per_tool_oldest_first() {
        let mut calls: Vec<McpCall> = (0..15).map(|i| call(&i.to_string())).collect();
        calls[0].status = McpCallStatus::Error;
        calls[14].tool = "other".into();
        let cells = recent_cells(&calls, 12);
        let mine = &cells["t"];
        assert_eq!(mine.len(), 12);
        assert_eq!(mine.last().map(|c| c.kind), Some(CellKind::Fault));
        assert_eq!(cells["other"].len(), 1);
    }

    #[test]
    fn empty_messages_fall_back() {
        assert_eq!(error_message("", "Failed"), "Failed");
        assert_eq!(error_message("boom", "Failed"), "boom");
    }
}
