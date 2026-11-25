//! Claude Code: the host link, live sessions with transcripts in the
//! inspector, and the `claude_*` action timeline grouped by session.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use leptos::html::Div;
use leptos::prelude::*;
use omni_api::claude::{
    ClaudeActivityResponse, ClaudeLinkView, ClaudeSession, ClaudeSessionsResponse,
    ClaudeTranscriptItem, ClaudeTranscriptResponse,
};
use omni_api::mcp_activity::{McpCall, McpCallStatus};
use omni_web_kit::api::{self, ApiClientError, ClaudeLinkError, ClaudeLinkGetError};
use omni_web_kit::components::{
    Button, ButtonLink, ButtonSize, ButtonVariant, CallStatusPill, Chip, Disclosure, EmptyState,
    ErrorState, Icon, InlineNote, Inspector, PageHead, Panel, Readout, ReadoutBand, ReadoutSize,
    ShowMoreButton, SkeletonRows, Status, StatusKind, Tag, Tone,
};
use omni_web_kit::hooks::{use_now, use_visible_poll};
use omni_web_kit::markdown::Markdown;
use omni_web_kit::task::spawn_scoped;
use omni_web_kit::utils::claude_activity::{
    ClaudeActionGroup, ClaudeActionKind, boolean_field, claude_action_kind, explain_claude_error,
    group_claude_actions, number_field, output_session, parse_iso_ms, short_session_id, snippet,
    string_field, summarize_compact_action, tool_input_summary,
};
use omni_web_kit::utils::format::{
    format_absolute, format_duration, format_relative_at, to_title_case,
};
use omni_web_kit::utils::js::{date_locale_time_string, locale_compare, number_string, utf16_len};

use crate::mcp::error_message;

const TIMELINE_PAGE: usize = 6;
const OTHER_PAGE: usize = 8;
/// Session groups shown before "Show more" (each can hold many actions).
const GROUP_PAGE: usize = 4;
const POLL: Duration = Duration::from_secs(10);

/// Opens the transcript for `(session_id, title)`.
type OpenTranscript = Callback<(String, String)>;

// ===== Link status =====

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Online,
    Offline,
    Disabled,
    Unconfigured,
}

impl LinkState {
    pub fn of(link: &ClaudeLinkView) -> Self {
        if !link.configured {
            Self::Unconfigured
        } else if link.disabled {
            Self::Disabled
        } else if link.online {
            Self::Online
        } else {
            Self::Offline
        }
    }

    fn status(self) -> (StatusKind, &'static str) {
        match self {
            Self::Online => (StatusKind::Ok, "Online"),
            Self::Offline => (StatusKind::Warn, "Offline"),
            Self::Disabled => (StatusKind::Warn, "Disabled"),
            Self::Unconfigured => (StatusKind::Idle, "Not linked"),
        }
    }

    /// `(sentence, explanation)`; never a raw API string.
    pub fn copy(self) -> (&'static str, &'static str) {
        match self {
            Self::Online => (
                "The Mac is online.",
                "It is polling Omni for Claude Code jobs, so session tools act right away.",
            ),
            Self::Offline => (
                "The Mac is offline.",
                "It has stopped polling. It may be asleep, away from home without VPN, or the omni-link agent stopped. Jobs sent now will not be picked up.",
            ),
            Self::Disabled => (
                "Claude tools are switched off.",
                "The kill switch is on, so Claude tools refuse to act. Run `omni-link enable` on the Mac to turn it back on.",
            ),
            Self::Unconfigured => (
                "The Mac isn't linked.",
                "Set OMNI_DEVICE_LINK_TOKEN on the server and run omni-link on the Mac to let agents start and steer Claude Code sessions.",
            ),
        }
    }
}

/// A session's state: busy is running, idle is the quiet default.
pub fn session_status(status: &str) -> (StatusKind, String) {
    let kind = match status {
        "busy" | "running" => StatusKind::Running,
        "idle" | "waiting" | "ready" => StatusKind::Ok,
        "stopped" | "exited" | "ended" => StatusKind::Idle,
        "error" | "failed" | "crashed" => StatusKind::Fault,
        _ => StatusKind::Info,
    };
    (kind, to_title_case(status))
}

fn link_error_notice(error: &ClaudeLinkError) -> impl IntoView + use<> {
    let hint = explain_claude_error(Some(&error.code)).hint;
    view! { <ErrorState title=error.message.clone() detail=hint warn=true /> }
}

/// The error the sessions route would return while the link is down.
fn link_down_error(state: LinkState) -> ClaudeLinkError {
    let (code, message) = match state {
        LinkState::Unconfigured => ("not_configured", "The Mac link is not configured"),
        LinkState::Disabled => ("disabled", "The Mac link is disabled"),
        LinkState::Offline | LinkState::Online => ("offline", "The Mac is offline"),
    };
    ClaudeLinkError {
        status: 503,
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn claude_get_error(error: ClaudeLinkGetError, fallback: &'static str) -> AnyView {
    match error {
        ClaudeLinkGetError::Link(link) => link_error_notice(&link).into_any(),
        ClaudeLinkGetError::Client(client) => {
            view! { <ErrorState title=fallback raw=client.message().to_owned() /> }.into_any()
        }
    }
}

// ===== Shared bits =====

fn status_view(status: Option<String>) -> Option<impl IntoView> {
    status.filter(|s| !s.is_empty()).map(|status| {
        let (kind, word) = session_status(&status);
        view! { <Status kind=kind label=word /> }
    })
}

/// Long prompts clamp to six lines with a toggle; `open` survives re-renders.
fn expandable_text(text: String, open: RwSignal<bool>) -> impl IntoView {
    let lines = 6;
    let long = utf16_len(&text) > lines * 90 || text.split('\n').count() > lines;
    view! {
        <div class="claude-quote-wrap">
            <div class=move || if long && !open.get() { "claude-quote clamped" } else { "claude-quote" }>{text}</div>
            {long
                .then(|| {
                    view! {
                        <button type="button" class="textlink small" on:click=move |_| open.update(|o| *o = !*o)>
                            {move || if open.get() { "Show less" } else { "Show all" }}
                        </button>
                    }
                })}
        </div>
    }
}

fn tag(text: impl Into<String>, tone: Tone) -> impl IntoView {
    let text = text.into();
    view! { <Tag tone=tone>{text}</Tag> }
}

// ===== Transcript =====

fn transcript_item(item: ClaudeTranscriptItem) -> impl IntoView {
    let time = parse_iso_ms(item.timestamp.as_deref());
    let foot = move || {
        let stamp = time.map(|time| {
            view! {
                <time title=format_absolute(time)>
                    {date_locale_time_string(time, &[("hour", "numeric"), ("minute", "2-digit")])}
                </time>
            }
        });
        let truncated = item.truncated.then(|| view! { <Tag>"truncated"</Tag> });
        view! { <div class="tx-foot mono small muted">{truncated} {stamp}</div> }
    };
    match item.kind.as_str() {
        "user" => view! {
            <div class="tx-row user">
                <div class="tx-bubble user">
                    <p class="tx-text">{item.text.clone().unwrap_or_default()}</p>
                    {foot()}
                </div>
            </div>
        }
        .into_any(),
        "assistant" => view! {
            <div class="tx-row">
                <div class="tx-bubble">
                    <Markdown content=item.text.clone().unwrap_or_default() />
                    {foot()}
                </div>
            </div>
        }
        .into_any(),
        "tool_use" => {
            let input = item.input.clone().filter(|i| !i.is_empty());
            let name = item.tool.clone().unwrap_or_else(|| "tool".to_owned());
            let summary = input
                .as_deref()
                .and_then(|i| tool_input_summary(Some(i)))
                .map(|s| format!("{name} · {s}"))
                .unwrap_or(name);
            view! {
                <div class="tx-row tool">
                    <Disclosure summary=summary class="tx-tool">
                        {input.map(|i| view! { <pre class="json-block">{i}</pre> })}
                    </Disclosure>
                </div>
            }
            .into_any()
        }
        "tool_result" => {
            let is_error = item.is_error.unwrap_or(false);
            let short = snippet(item.text.as_deref(), 80);
            let head = if is_error {
                "Tool error"
            } else {
                "Tool result"
            };
            let summary = [
                Some(head.to_owned()),
                item.tool.clone().filter(|t| !t.is_empty()),
                short,
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
            let truncated = if item.truncated { " (truncated)" } else { "" };
            view! {
                <div class=if is_error { "tx-row tool fault" } else { "tx-row tool" }>
                    <Disclosure summary=format!("{summary}{truncated}") class="tx-tool">
                        <pre class="json-block">{item.text.clone().unwrap_or_else(|| "(empty)".to_owned())}</pre>
                    </Disclosure>
                </div>
            }
            .into_any()
        }
        _ => view! {
            <div class="tx-row other small muted">
                <Tag>{item.kind.clone()}</Tag>
                " "
                {item.text.clone().unwrap_or_default()}
            </div>
        }
        .into_any(),
    }
}

#[component]
fn TranscriptInspector(session_id: String, title: String, on_close: Callback<()>) -> impl IntoView {
    let transcript = RwSignal::new(None::<ClaudeTranscriptResponse>);
    let error = RwSignal::new(None::<ClaudeLinkGetError>);
    let loading = RwSignal::new(true);
    let version = RwSignal::new(0u32);
    let end_ref = NodeRef::<Div>::new();

    let fetch_id = session_id.clone();
    Effect::new(move |_| {
        version.track();
        loading.set(true);
        let id = fetch_id.clone();
        spawn_scoped(async move {
            match api::fetch_claude_transcript(&id, 40, None).await {
                Ok(next) => {
                    transcript.set(Some(next));
                    error.set(None);
                }
                Err(err) => error.set(Some(err)),
            }
            loading.set(false);
        });
    });

    // Newest turns are at the bottom: show them first.
    Effect::new(move |_| {
        if transcript.with(Option::is_some) {
            request_animation_frame(move || {
                if let Some(end) = end_ref.get_untracked() {
                    end.scroll_into_view_with_bool(false);
                }
            });
        }
    });

    let short = short_session_id(&session_id);
    view! {
        <Inspector
            title=title
            wide=true
            on_close
            status=ViewFn::from(move || {
                let short = short.clone();
                view! {
                    <span class="mono small muted">{short}</span>
                    {move || transcript.with(|t| t.as_ref().map(|t| view! { <span class="mono small muted">{format!("rev {}", t.revision)}</span> }))}
                }
            })
            actions=ViewFn::from(move || view! {
                <Button size=ButtonSize::Sm icon=Icon::Refresh busy=loading on_click=Callback::new(move |_| version.update(|v| *v += 1))>
                    "Refresh"
                </Button>
            })
        >
            <section class="inspector-section tx">
                {move || {
                    transcript
                        .with(|t| t.as_ref().filter(|t| t.has_more).map(|t| t.items.len()))
                        .map(|count| {
                            view! {
                                <InlineNote>
                                    {format!("Showing the latest {count} items. Earlier history stays on the Mac.")}
                                </InlineNote>
                            }
                        })
                }}
                {move || error.get().map(|e| claude_get_error(e, "The transcript could not load"))}
                {move || {
                    (transcript.with(Option::is_none) && error.with(Option::is_none))
                        .then(|| view! { <SkeletonRows count=4 label="Loading transcript" /> })
                }}
                {move || {
                    transcript
                        .with(|t| t.as_ref().is_some_and(|t| t.items.is_empty()))
                        .then(|| view! { <EmptyState compact=true message="No transcript items yet." /> })
                }}
                {move || transcript.get().map(|t| t.items.into_iter().map(transcript_item).collect_view())}
                <div node_ref=end_ref></div>
            </section>
        </Inspector>
    }
}

// ===== Sessions =====

/// `[(name, count)]` sorted by `localeCompare`.
fn sorted_counts(counts: HashMap<String, usize>) -> Vec<(String, usize)> {
    let mut entries: Vec<(String, usize)> = counts.into_iter().collect();
    entries.sort_by(|a, b| locale_compare(&a.0, &b.0));
    entries
}

fn project_chips(projects: Vec<(String, usize)>, project: RwSignal<String>) -> impl IntoView {
    view! {
        <Chip
            pressed=Signal::derive(move || project.with(String::is_empty))
            on_click=Callback::new(move |()| project.set(String::new()))
        >
            "All"
        </Chip>
        {projects
            .into_iter()
            .map(|(name, count)| {
                let pressed_name = name.clone();
                let label = name.clone();
                view! {
                    <Chip
                        pressed=Signal::derive(move || project.with(|p| *p == pressed_name))
                        count=count
                        on_click=Callback::new(move |()| {
                            let next = name.clone();
                            project.update(|p| *p = if *p == next { String::new() } else { next });
                        })
                    >
                        {label}
                    </Chip>
                }
            })
            .collect_view()}
    }
}

#[component]
fn SessionsPanel(
    sessions: RwSignal<Option<Vec<ClaudeSession>>>,
    error: RwSignal<Option<ClaudeLinkGetError>>,
    include_stopped: RwSignal<bool>,
    now: ReadSignal<f64>,
    on_open_transcript: OpenTranscript,
) -> impl IntoView {
    let project = RwSignal::new(String::new());
    let projects = Memo::new(move |_| {
        let mut counts = HashMap::<String, usize>::new();
        sessions.with(|s| {
            for session in s.iter().flatten() {
                let key = session
                    .project
                    .clone()
                    .unwrap_or_else(|| "Other".to_owned());
                *counts.entry(key).or_default() += 1;
            }
        });
        sorted_counts(counts)
    });
    let visible = Memo::new(move |_| {
        let project = project.get();
        sessions.with(|s| {
            s.iter()
                .flatten()
                .filter(|session| {
                    project.is_empty() || session.project.as_deref().unwrap_or("Other") == project
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let busy = Memo::new(move |_| {
        sessions.with(|s| s.iter().flatten().filter(|s| s.status == "busy").count())
    });
    // Branch on memos so each poll updates the rows in place instead of
    // rebuilding the table (and an unchanged error its Details disclosure).
    let loaded = Memo::new(move |_| sessions.with(Option::is_some));
    let empty = Memo::new(move |_| visible.with(Vec::is_empty));
    let error_now = Memo::new(move |_| error.get());

    let row = move |session: ClaudeSession| {
        let started = parse_iso_ms(session.started_at.as_deref());
        let title = session
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Untitled session".to_owned());
        let (kind, word) = session_status(&session.status);
        let excerpt = session
            .last_assistant
            .clone()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "No assistant reply yet.".to_owned());
        let open = {
            let id = session.session_id.clone();
            let title = title.clone();
            move || on_open_transcript.run((id.clone(), title.clone()))
        };
        let open_key = open.clone();
        view! {
            <tr
                data-row="true"
                tabindex="0"
                class="clickable"
                title="Open the transcript"
                on:click=move |_| open()
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        open_key();
                    }
                }
            >
                <td class="cell-status"><Status kind=kind label=word.clone() dot_only=kind == StatusKind::Ok /></td>
                <td class="grow">
                    <div class="cell-two">
                        <span class="truncate strong">{title.clone()}</span>
                        <span class="truncate small muted">{excerpt}</span>
                    </div>
                </td>
                <td class="hide-below-desk">
                    {session.project.clone().filter(|p| !p.is_empty()).map(|p| view! { <Tag>{p}</Tag> })}
                </td>
                <td class="numeric hide-below-wide muted">{format!("rev {}", session.revision)}</td>
                <td class="numeric nowrap" title=started.map(format_absolute)>
                    {move || started.map_or_else(|| "—".to_owned(), |at| format_relative_at(at, now.get()))}
                </td>
            </tr>
        }
    };

    view! {
        <Panel
            title="Sessions"
            class="claude-sessions"
            head_end=ViewFn::from(move || view! {
                {move || {
                    let busy = busy.get();
                    (busy > 0).then(|| view! { <Status kind=StatusKind::Running label=format!("{busy} busy") /> })
                }}
                <span class="num">{move || sessions.with(|s| s.as_ref().map(Vec::len))}</span>
            })
        >
            <div class="panel-body claude-filters">
                <div class="chips" role="group" aria-label="Filter by project">
                    {move || project_chips(projects.get(), project)}
                </div>
                <Chip
                    pressed=include_stopped
                    on_click=Callback::new(move |()| include_stopped.update(|v| *v = !*v))
                >
                    "Include stopped"
                </Chip>
            </div>
            {move || error_now.get().map(|e| view! { <div class="panel-body">{claude_get_error(e, "Sessions could not load")}</div> })}
            {move || {
                if !loaded.get() {
                    return error_now.with(Option::is_none).then(|| view! { <SkeletonRows count=3 label="Asking the Mac" /> }.into_any());
                }
                if empty.get() {
                    let message = if include_stopped.get() { "No sessions." } else { "No running sessions." };
                    return Some(view! { <EmptyState compact=true message=message /> }.into_any());
                }
                Some(view! {
                    <div class="table-wrap">
                        <table class="table claude-session-table" data-primary-rows="true">
                            <thead>
                                <tr>
                                    <th class="cell-status"><span class="sr-only">"State"</span></th>
                                    <th>"Session"</th>
                                    <th class="hide-below-desk">"Project"</th>
                                    <th class="numeric hide-below-wide">"Revision"</th>
                                    <th class="numeric">"Started"</th>
                                </tr>
                            </thead>
                            <tbody>
                                <For
                                    each=move || visible.get()
                                    key=|session| format!("{}|{session:?}", session.session_id)
                                    children=row
                                />
                            </tbody>
                        </table>
                    </div>
                }
                .into_any())
            }}
        </Panel>
    }
}

// ===== Action timeline =====

fn error_callout(error: &str) -> impl IntoView + use<> {
    let explained = explain_claude_error(Some(error));
    let warn = explained.code.as_deref() == Some("outcome_unknown");
    view! {
        <div class=if warn { "claude-error warn" } else { "claude-error" }>
            {explained.code.map(|code| view! { <span class="mono small">{code}</span> })}
            <span class="claude-error-message">{error.to_owned()}</span>
            {explained.hint.map(|hint| view! { <span class="small dim">{hint}</span> })}
        </div>
    }
}

/// UI state that must survive the 10 s activity polls.
#[derive(Clone, Copy)]
struct TimelineState {
    /// Group keys whose earlier actions are revealed.
    expanded_groups: RwSignal<HashSet<String>>,
    /// Call ids whose prompt text is unclamped.
    open_texts: RwSignal<HashSet<String>>,
}

impl TimelineState {
    fn text_open(self, call_id: &str) -> RwSignal<bool> {
        let id = call_id.to_owned();
        let open = RwSignal::new(self.open_texts.with_untracked(|s| s.contains(&id)));
        let texts = self.open_texts;
        Effect::new(move |_| {
            let value = open.get();
            texts.update(|set| {
                if value {
                    set.insert(id.clone());
                } else {
                    set.remove(&id);
                }
            });
        });
        open
    }
}

fn action_body(call: &McpCall, state: TimelineState) -> AnyView {
    let kind = claude_action_kind(&call.tool, &call.input);
    let input = &call.input;
    let output = &call.output;
    let session = output_session(call);
    let prompt = string_field(input, "prompt");
    let prompt_view =
        |prompt: Option<String>| prompt.map(|p| expandable_text(p, state.text_open(&call.call_id)));
    match kind {
        ClaudeActionKind::Start => {
            let model = string_field(input, "model");
            let effort = string_field(input, "effort");
            let title = string_field(input, "title");
            let project = string_field(input, "project");
            let reused = boolean_field(output, "reused") == Some(true);
            view! {
                <div class="cluster">
                    {project.map(|p| tag(p, Tone::Neutral))}
                    {model.map(|m| tag(m, Tone::Neutral))}
                    {effort.map(|e| tag(format!("effort {e}"), Tone::Neutral))}
                    {reused.then(|| tag("reused", Tone::Signal))}
                </div>
                {title.map(|t| view! { <p class="claude-line">{t}</p> })}
                {prompt_view(prompt)}
            }
            .into_any()
        }
        ClaudeActionKind::Send => {
            let warning = string_field(output, "warning");
            let interrupt = boolean_field(input, "interrupt") == Some(true);
            view! {
                {interrupt.then(|| view! { <div class="cluster">{tag("interrupt", Tone::Warn)}</div> })}
                {prompt_view(prompt)}
                {warning.map(|w| view! { <InlineNote tone=Tone::Warn>{w}</InlineNote> })}
            }
            .into_any()
        }
        ClaudeActionKind::Wait => {
            let Some(timed_out) = boolean_field(output, "timedOut") else {
                return ().into_any();
            };
            let timeout = number_field(input, "timeoutSeconds");
            let status = session.as_ref().and_then(|s| s.status.clone());
            let revision = session.as_ref().and_then(|s| s.revision);
            if timed_out {
                view! {
                    <p class="claude-line">
                        {tag("timed out", Tone::Warn)}
                        {timeout.map(|t| format!(" after {}s", number_string(t)))}
                        {status.map(|s| format!(", still {s}"))}
                    </p>
                }
                .into_any()
            } else {
                view! {
                    <p class="claude-line">
                        {tag("settled", Tone::Ok)}
                        {status.map(|s| format!(" {s}"))}
                        {revision.map(|r| format!(" at rev {}", number_string(r)))}
                    </p>
                }
                .into_any()
            }
        }
        ClaudeActionKind::Result => {
            if output.is_null() {
                return ().into_any();
            }
            match string_field(output, "result") {
                Some(result) => {
                    let truncated = boolean_field(output, "truncated") == Some(true);
                    view! {
                        <div class="claude-result">
                            <Markdown content=result />
                            {truncated.then(|| tag("truncated", Tone::Neutral))}
                        </div>
                    }
                    .into_any()
                }
                None => view! { <p class="claude-line muted">"No result yet."</p> }.into_any(),
            }
        }
        ClaudeActionKind::Stop => ().into_any(),
        _ => view! { <p class="claude-line muted">{summarize_compact_action(call)}</p> }.into_any(),
    }
}

fn marker_class(status: McpCallStatus) -> &'static str {
    match status {
        McpCallStatus::Running => "tl-item running",
        McpCallStatus::Ok => "tl-item",
        McpCallStatus::Error => "tl-item fault",
        McpCallStatus::Interrupted | McpCallStatus::Invalid => "tl-item warn",
    }
}

fn timeline_item(call: McpCall, now: ReadSignal<f64>, state: TimelineState) -> impl IntoView {
    let kind = claude_action_kind(&call.tool, &call.input);
    let started = call.started_at as f64;
    let duration = call
        .duration_ms
        .map(|d| format!(" · {}", format_duration(d as f64)))
        .unwrap_or_default();
    view! {
        <li class=marker_class(call.status)>
            <div class="tl-head">
                <span class="tl-label">{kind.label()}</span>
                {(call.status != McpCallStatus::Ok).then(|| view! { <CallStatusPill status=call.status /> })}
                <span class="tl-time mono small muted" title=format_absolute(started)>
                    {move || format_relative_at(started, now.get())}
                    {duration}
                </span>
            </div>
            {call.error.as_deref().filter(|e| !e.is_empty()).map(error_callout)}
            {action_body(&call, state)}
        </li>
    }
}

fn action_group(
    group: ClaudeActionGroup,
    now: ReadSignal<f64>,
    on_open: OpenTranscript,
    state: TimelineState,
) -> impl IntoView {
    let key = group.key.clone();
    let total = group.actions.len();
    let title = group.title.clone().unwrap_or_else(|| {
        if group.session_id.is_some() {
            "Untitled session".to_owned()
        } else {
            "Failed start".to_owned()
        }
    });
    let hidden_count = {
        let key = key.clone();
        Memo::new(move |_| {
            if state.expanded_groups.with(|s| s.contains(&key)) {
                0
            } else {
                total.saturating_sub(TIMELINE_PAGE)
            }
        })
    };
    let actions = group.actions.clone();
    let open_title = title.clone();
    let latest = group.latest_at as f64;
    let transcript = group.session_id.clone().map(|id| {
        view! {
            <Button
                size=ButtonSize::Sm
                variant=ButtonVariant::Ghost
                icon=Icon::Terminal
                on_click=Callback::new(move |_| on_open.run((id.clone(), open_title.clone())))
            >
                "Transcript"
            </Button>
        }
    });
    view! {
        <Panel class=if group.latest_failed { "claude-group failed" } else { "claude-group" }>
            <div class="claude-group-head">
                <div class="claude-group-id">
                    <div class="cluster">
                        <h3 class="claude-group-title">{title}</h3>
                        {status_view(group.status.clone())}
                    </div>
                    <div class="cluster small muted">
                        {group.project.clone().map(|p| view! { <Tag>{p}</Tag> })}
                        {group
                            .session_id
                            .clone()
                            .map(|id| view! { <span class="mono" title=id.clone()>{short_session_id(&id)}</span> })}
                        <span>{format!("{total} action{}", if total == 1 { "" } else { "s" })}</span>
                        <span class="mono" title=format_absolute(latest)>
                            {move || format_relative_at(latest, now.get())}
                        </span>
                    </div>
                </div>
                {transcript}
            </div>
            {move || {
                let hidden = hidden_count.get();
                (hidden > 0)
                    .then(|| {
                        let key = key.clone();
                        view! {
                            <button
                                type="button"
                                class="claude-earlier small"
                                on:click=move |_| state.expanded_groups.update(|s| { s.insert(key.clone()); })
                            >
                                {format!("Show {hidden} earlier action{}", if hidden == 1 { "" } else { "s" })}
                            </button>
                        }
                    })
            }}
            <ol class="tl">
                {move || {
                    actions
                        .iter()
                        .skip(hidden_count.get())
                        .cloned()
                        .map(|call| timeline_item(call, now, state))
                        .collect_view()
                }}
            </ol>
        </Panel>
    }
}

fn other_actions(
    calls: Vec<McpCall>,
    now: ReadSignal<f64>,
    shown: RwSignal<usize>,
) -> impl IntoView {
    let total = calls.len();
    let rows = move || {
        calls
            .iter()
            .take(shown.get())
            .map(|call| {
                let kind = claude_action_kind(&call.tool, &call.input);
                let started = call.started_at as f64;
                let summary = match call.error.clone().filter(|e| !e.is_empty()) {
                    Some(error) => view! { <span class="truncate text-fault">{error}</span> }.into_any(),
                    None => view! { <span class="truncate dim">{summarize_compact_action(call)}</span> }.into_any(),
                };
                let status = call.status;
                view! {
                    <tr>
                        <td class="nowrap strong">{kind.label()}</td>
                        <td class="grow"><div class="cell-two">{summary}</div></td>
                        <td class="cell-status">
                            {(status != McpCallStatus::Ok).then(|| view! { <CallStatusPill status=status /> })}
                        </td>
                        <td class="numeric nowrap" title=format_absolute(started)>
                            {move || format_relative_at(started, now.get())}
                        </td>
                    </tr>
                }
            })
            .collect_view()
    };
    let remaining = Signal::derive(move || total.saturating_sub(shown.get()));
    view! {
        <Panel title="Other calls" head_end=ViewFn::from(|| view! { <span>"Listings and status checks"</span> })>
            <div class="table-wrap">
                <table class="table dense">
                    <tbody>{rows}</tbody>
                </table>
            </div>
            {move || {
                (remaining.get() > 0)
                    .then(|| {
                        view! {
                            <div class="panel-foot claude-more">
                                <ShowMoreButton remaining on_click=Callback::new(move |()| shown.update(|n| *n += OTHER_PAGE)) />
                            </div>
                        }
                    })
            }}
        </Panel>
    }
}

#[component]
fn ActionsSection(
    #[prop(into)] actions: Signal<Vec<McpCall>>,
    now: ReadSignal<f64>,
    on_open_transcript: OpenTranscript,
) -> impl IntoView {
    let project = RwSignal::new(String::new());
    let state = TimelineState {
        expanded_groups: RwSignal::new(HashSet::new()),
        open_texts: RwSignal::new(HashSet::new()),
    };
    let other_shown = RwSignal::new(OTHER_PAGE);
    let groups_shown = RwSignal::new(GROUP_PAGE);
    let grouped = Memo::new(move |_| actions.with(|a| group_claude_actions(a)));
    let projects = Memo::new(move |_| {
        let mut counts = HashMap::<String, usize>::new();
        grouped.with(|g| {
            for group in &g.sessions {
                if let Some(project) = group.project.clone().filter(|p| !p.is_empty()) {
                    *counts.entry(project).or_default() += 1;
                }
            }
        });
        sorted_counts(counts)
    });
    let visible = Memo::new(move |_| {
        let project = project.get();
        grouped.with(|g| {
            g.sessions
                .iter()
                .filter(|group| {
                    project.is_empty() || group.project.as_deref() == Some(project.as_str())
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    let other = Memo::new(move |_| grouped.with(|g| g.other.clone()));
    let shown_groups = Memo::new(move |_| {
        let limit = groups_shown.get();
        visible.with(|v| v.iter().take(limit).cloned().collect::<Vec<_>>())
    });
    let hidden_groups =
        Signal::derive(move || visible.with(Vec::len).saturating_sub(groups_shown.get()));
    // `actions` changes on every poll; only emptiness may rebuild the timeline.
    let empty = Memo::new(move |_| actions.with(Vec::is_empty));

    view! {
        <section class="section">
            <div class="section-head">
                <h2 class="section-title">"Actions"</h2>
                <span class="section-meta num">{move || actions.with(Vec::len)}</span>
                <div class="section-end">
                    <ButtonLink to="/mcp-activity" variant=ButtonVariant::Ghost size=ButtonSize::Sm>"All MCP calls"</ButtonLink>
                </div>
            </div>
            {move || {
                let projects = projects.get();
                (!projects.is_empty())
                    .then(|| view! { <div class="chips claude-projects" role="group" aria-label="Filter actions by project">{project_chips(projects, project)}</div> })
            }}
            {move || {
                if empty.get() {
                    view! { <EmptyState compact=true message="No Claude Code actions recorded yet." icon=Icon::Terminal /> }.into_any()
                } else {
                    view! {
                        <div class="stack">
                            <For
                                each=move || shown_groups.get()
                                key=|group| format!("{}|{group:?}", group.key)
                                children=move |group| action_group(group, now, on_open_transcript, state)
                            />
                            {move || (hidden_groups.get() > 0).then(|| view! {
                                <ShowMoreButton
                                    remaining=hidden_groups
                                    noun="sessions"
                                    on_click=Callback::new(move |()| groups_shown.update(|n| *n += GROUP_PAGE))
                                />
                            })}
                            {move || {
                                let other = other.get();
                                (project.with(String::is_empty) && !other.is_empty())
                                    .then(|| other_actions(other, now, other_shown))
                            }}
                        </div>
                    }
                        .into_any()
                }
            }}
        </section>
    }
}

// ===== Page =====

#[component]
fn LinkHero(
    link: Memo<Option<ClaudeLinkView>>,
    busy: Memo<usize>,
    now: ReadSignal<f64>,
) -> impl IntoView {
    move || {
        let link = link.get()?;
        let state = LinkState::of(&link);
        let (kind, word) = state.status();
        let last_seen = parse_iso_ms(link.last_seen_at.as_deref());
        let pending = link.pending_jobs;
        let setup = (state == LinkState::Unconfigured).then(|| {
            view! {
                <Panel title="Link the Mac" pad=true class="claude-setup">
                    <ol class="claude-steps">
                        <li>"Set " <code>"OMNI_DEVICE_LINK_TOKEN"</code> " in the server environment and redeploy."</li>
                        <li>"Install the dotfiles " <code>"omni-link"</code> " agent on the Mac with the same token."</li>
                        <li>"Start it; this page turns online within a few seconds."</li>
                    </ol>
                </Panel>
            }
        });
        Some(view! {
            <ReadoutBand cols=4 aria_label="Mac link" class="claude-link">
                <Readout label="Link" value=word.to_owned() size=ReadoutSize::M>
                    <Status kind=kind label=word />
                </Readout>
                <Readout label="Host" value=link.host.clone().unwrap_or_else(|| "—".to_owned()) size=ReadoutSize::M class="claude-host" />
                <Readout
                    label="Last seen"
                    value=Signal::derive(move || last_seen.map_or_else(|| "Never".to_owned(), |at| format_relative_at(at, now.get())))
                    size=ReadoutSize::M
                    title=last_seen.map(format_absolute)
                />
                <Readout
                    label="Pending jobs"
                    value=pending.to_string()
                    size=ReadoutSize::M
                    tone={if pending > 0 && state != LinkState::Online { Tone::Warn } else { Tone::Neutral }}
                >
                    {move || format!("{} busy session{}", busy.get(), if busy.get() == 1 { "" } else { "s" })}
                </Readout>
            </ReadoutBand>
            {setup}
        })
    }
}

#[component]
pub fn ClaudePage() -> impl IntoView {
    let now = use_now(15_000);
    let activity = RwSignal::new(None::<ClaudeActivityResponse>);
    let error = RwSignal::new(None::<String>);
    let transcript = RwSignal::new(None::<(String, String)>);
    let include_stopped = RwSignal::new(false);
    let sessions = RwSignal::new(None::<Vec<ClaudeSession>>);
    let sessions_error = RwSignal::new(None::<ClaudeLinkGetError>);

    let reload = use_visible_poll(
        Signal::stored("activity".to_owned()),
        || api::fetch_claude_activity(200),
        move |response: ClaudeActivityResponse| {
            activity.set(Some(response));
            error.set(None);
        },
        move |err: ApiClientError| {
            error.set(Some(error_message(
                err.message(),
                "Failed to load Claude activity",
            )));
        },
        POLL,
    );
    // The sessions route needs the host link: wait for the first activity
    // read to say whether it is up, and while it reports the link down (or
    // not configured) skip the request and show the same state locally.
    let link_down = Memo::new(move |_| {
        activity.with(|a| {
            a.as_ref()
                .map(|a| Some(LinkState::of(&a.link)).filter(|state| *state != LinkState::Online))
        })
    });
    use_visible_poll(
        Signal::derive(move || format!("sessions:{}:{:?}", include_stopped.get(), link_down.get())),
        move || {
            let include = include_stopped.get_untracked();
            let gate = link_down.get_untracked();
            async move {
                match gate {
                    // Unknown yet: the key changes once activity loads,
                    // which restarts this loop.
                    None => futures::future::pending().await,
                    Some(Some(state)) => Err(ClaudeLinkGetError::Link(link_down_error(state))),
                    Some(None) => api::fetch_claude_sessions(include, 25).await,
                }
            }
        },
        move |response: ClaudeSessionsResponse| {
            sessions.set(Some(response.sessions));
            sessions_error.set(None);
        },
        move |err: ClaudeLinkGetError| {
            // The host is unreachable; a stale list would suggest it is still live.
            if matches!(err, ClaudeLinkGetError::Link(_)) {
                sessions.set(None);
            }
            sessions_error.set(Some(err));
        },
        POLL,
    );

    let open_transcript: OpenTranscript =
        Callback::new(move |(session_id, title): (String, String)| {
            transcript.set(Some((session_id, title)))
        });
    let has_activity = Memo::new(move |_| activity.with(Option::is_some));
    let link = Memo::new(move |_| activity.with(|a| a.as_ref().map(|a| a.link.clone())));
    let state = Memo::new(move |_| link.with(|l| l.as_ref().map(LinkState::of)));
    let busy = Memo::new(move |_| {
        sessions.with(|s| s.iter().flatten().filter(|s| s.status == "busy").count())
    });
    let actions = Signal::derive(move || {
        activity.with(|a| a.as_ref().map(|a| a.actions.clone()).unwrap_or_default())
    });
    let title =
        Signal::derive(move || state.get().map_or("Claude Code", |s| s.copy().0).to_owned());
    let lede = Signal::derive(move || state.get().map(|s| s.copy().1.to_owned()));
    // Failed polls set the same message again; only a change may rebuild the
    // error state (its Details disclosure would collapse).
    let error_text = Memo::new(move |_| error.get());

    view! {
        <PageHead
            title
            sentence=true
            lede
            actions=ViewFn::from(|| view! {
                <ButtonLink to="/mcp-activity" icon=Icon::Plug>"MCP activity"</ButtonLink>
            })
        />
        {move || {
            if has_activity.get() {
                return error_text
                    .get()
                    .map(|e| view! {
                        <InlineNote tone=Tone::Warn role="status">
                            {format!("Refresh failed ({e}). Showing the last loaded state.")}
                        </InlineNote>
                    }
                    .into_any());
            }
            Some(match error_text.get() {
                Some(e) => view! {
                    <ErrorState
                        title="Claude activity could not load"
                        raw=e
                        retry=Callback::new(move |()| reload.run(()))
                    />
                }
                .into_any(),
                None => view! { <SkeletonRows count=3 label="Loading Claude activity" /> }.into_any(),
            })
        }}
        <div class="stack-lg claude-page">
            <LinkHero link busy now />
            <SessionsPanel
                sessions
                error=sessions_error
                include_stopped
                now
                on_open_transcript=open_transcript
            />
            {move || {
                has_activity
                    .get()
                    .then(|| view! { <ActionsSection actions=actions now=now on_open_transcript=open_transcript /> })
            }}
        </div>
        {move || {
            transcript
                .get()
                .map(|(session_id, title)| {
                    view! {
                        <TranscriptInspector
                            session_id=session_id
                            title=title
                            on_close=Callback::new(move |()| transcript.set(None))
                        />
                    }
                })
        }}
    }
}

#[cfg(test)]
mod tests {
    use omni_api::claude::ClaudeLinkView;
    use omni_web_kit::components::StatusKind;

    use super::{LinkState, session_status};

    fn link(configured: bool, disabled: bool, online: bool) -> ClaudeLinkView {
        ClaudeLinkView {
            configured,
            online,
            disabled,
            host: None,
            last_seen_at: None,
            pending_jobs: 0,
        }
    }

    #[test]
    fn link_state_prefers_configuration_then_kill_switch() {
        assert_eq!(
            LinkState::of(&link(false, true, true)),
            LinkState::Unconfigured
        );
        assert_eq!(LinkState::of(&link(true, true, true)), LinkState::Disabled);
        assert_eq!(LinkState::of(&link(true, false, true)), LinkState::Online);
        assert_eq!(LinkState::of(&link(true, false, false)), LinkState::Offline);
        assert!(!LinkState::Offline.copy().0.contains("error"));
    }

    #[test]
    fn session_states_map_to_status_shapes() {
        assert_eq!(
            session_status("busy"),
            (StatusKind::Running, "Busy".to_owned())
        );
        assert_eq!(session_status("idle").0, StatusKind::Ok);
        assert_eq!(session_status("stopped").0, StatusKind::Idle);
        assert_eq!(session_status("mystery").0, StatusKind::Info);
    }
}
