//! The integration inspector: what it serves (tools with visibility,
//! resources, events), its last refresh and its settings.

use std::collections::BTreeSet;

use leptos::prelude::*;
use omni_api::gateway::{
    AuthKind, GatewayToolView, IntegrationStatus, IntegrationSummary, ToolChange, TransportKind,
};
use omni_web_kit::api;
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, Disclosure, EmptyState, ErrorState, Icon, InlineNote,
    Inspector, SearchField, SegOption, Segmented, ShowMoreButton, SkeletonRows, Status, StatusKind,
    Tag, ToastKind, Tone, use_show_more,
};
use omni_web_kit::task::spawn_detached;
use omni_web_kit::utils::format::{format_absolute, format_duration};

use super::editor::{CredentialsSection, DeleteSection, SettingsForm};
use super::integrations::{IntegrationStatusChip, IssueLine};
use super::logic::{self, SettingsDraft};
use super::{CopyButton, Gateway};

/// Tools shown before "Show more".
const TOOL_PAGE: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DetailTab {
    Tools,
    Resources,
    Events,
    Settings,
}

/// The freshest summary: the polled overview's copy, else the detail's.
fn current_summary(gw: Gateway) -> Memo<Option<IntegrationSummary>> {
    Memo::new(move |_| {
        let id = gw.selected.get()?;
        gw.overview
            .with(|o| {
                o.as_ref()?
                    .integrations
                    .iter()
                    .find(|i| i.id == id)
                    .cloned()
            })
            .or_else(|| {
                gw.detail.with(|d| {
                    d.as_ref()
                        .filter(|d| d.summary.id == id)
                        .map(|d| d.summary.clone())
                })
            })
    })
}

#[component]
pub(super) fn IntegrationInspector(gw: Gateway, on_close: Callback<()>) -> impl IntoView {
    let summary = current_summary(gw);
    let id = Memo::new(move |_| gw.selected.get().unwrap_or_default());
    let title = Signal::derive(move || {
        summary
            .with(|s| s.as_ref().map(|s| s.name.clone()))
            .unwrap_or_else(|| id.get())
    });
    let status = Signal::derive(move || {
        summary.with(|s| s.as_ref().map_or(IntegrationStatus::Pending, |s| s.status))
    });
    let enabled = Memo::new(move |_| summary.with(|s| s.as_ref().is_some_and(|s| s.enabled)));
    let refreshing = Signal::derive(move || gw.is_refreshing(&id.get()));
    let toggling = Signal::derive(move || {
        gw.toggling
            .with(|t| t.as_deref() == Some(id.get().as_str()))
    });
    let needs_connect = Memo::new(move |_| {
        summary.with(|s| s.as_ref().is_some_and(|s| s.auth == AuthKind::OAuth))
            && gw.detail.with(|d| {
                d.as_ref()
                    .is_some_and(|d| !d.config.oauth.as_ref().is_some_and(|o| o.connected))
            })
    });
    let connecting = Signal::derive(move || {
        gw.connecting
            .with(|c| c.as_deref() == Some(id.get().as_str()))
    });
    let has_detail = Memo::new(move |_| gw.detail.with(Option::is_some));
    let error_text = Memo::new(move |_| gw.detail_error.get());

    let status_view = ViewFn::from(move || {
        view! { <IntegrationStatusChip gw id=id.get_untracked() status refreshing/> }
    });
    let actions = ViewFn::from(move || {
        view! {
            {move || needs_connect.get().then(|| view! {
                <Button
                    variant=ButtonVariant::Primary
                    icon=Icon::External
                    busy=connecting
                    on_click=Callback::new(move |_| gw.connect(id.get_untracked()))
                >
                    "Connect"
                </Button>
            })}
            <Button
                icon=Icon::Refresh
                busy=refreshing
                disabled=Signal::derive(move || !enabled.get())
                disabled_reason="Enable the integration to refresh it"
                title="Read its tools, resources and events again"
                on_click=Callback::new(move |_| gw.refresh(id.get_untracked()))
            >
                "Refresh"
            </Button>
            <Button
                variant=ButtonVariant::Ghost
                busy=toggling
                on_click=Callback::new(move |_| gw.set_enabled(id.get_untracked(), !enabled.get_untracked()))
            >
                {move || if enabled.get() { "Disable" } else { "Enable" }}
            </Button>
        }
    });

    view! {
        <Inspector title status=status_view actions on_close wide=true>
            {move || {
                if has_detail.get() {
                    return view! {
                        {move || error_text.get().map(|e| view! {
                            <div class="inspector-section">
                                <InlineNote tone=Tone::Warn role="status">{format!("Reload failed ({e}). Showing the last loaded state.")}</InlineNote>
                            </div>
                        })}
                        <DetailBody gw/>
                    }
                    .into_any();
                }
                match error_text.get() {
                    Some(e) => view! {
                        <div class="inspector-section">
                            <ErrorState
                                title="This integration could not load"
                                raw=e
                                retry=Callback::new(move |()| gw.detail_nonce.update(|n| *n += 1))
                            />
                        </div>
                    }
                    .into_any(),
                    None => view! { <SkeletonRows count=6 label="Loading the integration"/> }.into_any(),
                }
            }}
        </Inspector>
    }
}

/// Everything below the head. Drafts live here so switching tabs keeps them.
#[component]
fn DetailBody(gw: Gateway) -> impl IntoView {
    let hidden_draft = RwSignal::new(None::<BTreeSet<String>>);
    let query = RwSignal::new(String::new());
    let settings = RwSignal::new(None::<SettingsDraft>);
    let summary = current_summary(gw);
    let counts = Memo::new(move |_| {
        gw.detail.with(|d| {
            d.as_ref().map_or((0, 0, 0), |d| {
                (
                    d.tools.len(),
                    d.summary.resource_count as usize,
                    d.events.len(),
                )
            })
        })
    });
    let tab_options = Signal::derive(move || {
        let (tools, resources, events) = counts.get();
        vec![
            SegOption::new(DetailTab::Tools, "Tools").with_count(tools),
            SegOption::new(DetailTab::Resources, "Resources").with_count(resources),
            SegOption::new(DetailTab::Events, "Events").with_count(events),
            SegOption::new(DetailTab::Settings, "Settings"),
        ]
    });
    view! {
        <section class="inspector-section stack">
            <IssueLine gw summary=summary.into()/>
            <Facts gw/>
            <RefreshReportView gw/>
        </section>
        <div class="inspector-section gw-tabs">
            <Segmented
                options=tab_options
                value=gw.tab
                on_change=Callback::new(move |tab| gw.tab.set(tab))
                aria_label="Integration sections"
                small=true
                fill=true
            />
        </div>
        {move || match gw.tab.get() {
            DetailTab::Tools => view! { <ToolsTab gw hidden_draft query/> }.into_any(),
            DetailTab::Resources => view! { <ResourcesTab gw/> }.into_any(),
            DetailTab::Events => view! { <EventsTab gw/> }.into_any(),
            DetailTab::Settings => view! {
                <SettingsForm gw draft=settings/>
                <CredentialsSection gw/>
                <DeleteSection gw/>
            }
            .into_any(),
        }}
    }
}

#[component]
fn Facts(gw: Gateway) -> impl IntoView {
    move || {
        let now = gw.now.get();
        gw.detail.with(|d| {
            let d = d.as_ref()?;
            let s = &d.summary;
            let server = d.server.as_ref();
            let server_name = server
                .and_then(|v| v.name.clone())
                .map(|name| match server.and_then(|v| v.version.as_deref()) {
                    Some(version) => format!("{name} {version}"),
                    None => name,
                });
            let protocol = server.and_then(|v| v.protocol_version.clone());
            let endpoint_label = match s.transport {
                TransportKind::Http => "URL",
                TransportKind::Stdio => "Bundled server",
            };
            let auth = match (s.auth, d.config.credentials_set) {
                (AuthKind::None, _) => "None".to_owned(),
                (kind, true) => logic::auth_label(kind).to_owned(),
                (kind, false) => format!("{} · credentials missing", logic::auth_label(kind)),
            };
            let auth_class = if d.config.credentials_set || s.auth == AuthKind::None {
                ""
            } else {
                "text-warn"
            };
            let relative = |at: Option<i64>| {
                at.map_or_else(
                    || "Never".to_owned(),
                    |at| logic::ago(at as f64, now),
                )
            };
            let absolute = |at: Option<i64>| at.map(|at| format_absolute(at as f64));
            let last_refresh = d.last_refresh.as_ref().map(|r| {
                format!(
                    " · {}{}",
                    format_duration(r.duration_ms as f64),
                    if r.ok { "" } else { " · failed" }
                )
            });
            Some(view! {
                <dl class="kv">
                    <dt>"Id"</dt>
                    <dd class="mono">{s.id.clone()}</dd>
                    <dt>{endpoint_label}</dt>
                    <dd class="mono">{s.endpoint.clone()}</dd>
                    <dt>"Auth"</dt>
                    <dd class=auth_class>{auth}</dd>
                    <dt>"Server"</dt>
                    <dd>{server_name.unwrap_or_else(|| "Not read yet".to_owned())}</dd>
                    {protocol.map(|p| view! { <dt>"Protocol"</dt><dd class="mono">{p}</dd> })}
                    <dt>"Serving"</dt>
                    <dd class="num">{format!("{} of {} tools", s.visible_tool_count, s.tool_count)}</dd>
                    <dt>"Timeout"</dt>
                    <dd class="num">{format!("{} s", d.config.timeout_seconds)}</dd>
                    <dt>"Last refresh"</dt>
                    <dd class="num" title=absolute(s.last_refresh_at)>
                        {relative(s.last_refresh_at)}
                        {last_refresh}
                    </dd>
                    {(s.last_success_at != s.last_refresh_at).then(|| view! {
                        <dt>"Last success"</dt>
                        <dd class="num" title=absolute(s.last_success_at)>{relative(s.last_success_at)}</dd>
                    })}
                </dl>
            })
        })
    }
}

/// Warnings stay visible; the added, removed and changed names fold away.
#[component]
fn RefreshReportView(gw: Gateway) -> impl IntoView {
    let report = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().and_then(|d| d.last_refresh.clone()))
    });
    move || {
        let report = report.get()?;
        let warnings = report.warnings.clone();
        let groups: Vec<(&'static str, Vec<String>)> = [
            ("Added", report.added),
            ("Changed", report.changed),
            ("Removed", report.removed),
        ]
        .into_iter()
        .filter(|(_, names)| !names.is_empty())
        .collect();
        let summary = groups
            .iter()
            .map(|(label, names)| format!("{} {}", names.len(), label.to_lowercase()))
            .collect::<Vec<_>>()
            .join(", ");
        Some(view! {
            {warnings.into_iter().map(|w| view! { <InlineNote tone=Tone::Warn>{w}</InlineNote> }).collect_view()}
            {(!groups.is_empty()).then(|| view! {
                <Disclosure summary=format!("Last refresh: {summary}") flush=true>
                    <div class="stack">
                        {groups.into_iter().map(|(label, names)| view! {
                            <div class="stack gw-change-group">
                                <span class="label">{label}</span>
                                <div class="chips">
                                    {names.into_iter().map(|n| view! { <span class="tag">{n}</span> }).collect_view()}
                                </div>
                            </div>
                        }).collect_view()}
                    </div>
                </Disclosure>
            })}
        })
    }
}

// ----- tools -----

#[component]
fn ToolsTab(
    gw: Gateway,
    hidden_draft: RwSignal<Option<BTreeSet<String>>>,
    query: RwSignal<String>,
) -> impl IntoView {
    let tools = Memo::new(move |_| {
        gw.detail
            .with(|d| d.as_ref().map(|d| d.tools.clone()).unwrap_or_default())
    });
    let saved = Memo::new(move |_| tools.with(|t| logic::hidden_tools(t)));
    let hidden = Memo::new(move |_| hidden_draft.get().unwrap_or_else(|| saved.get()));
    let changes = Memo::new(move |_| {
        hidden_draft.with(|d| {
            d.as_ref()
                .map_or(0, |d| saved.with(|s| d.symmetric_difference(s).count()))
        })
    });
    let filtered = Memo::new(move |_| {
        query.with(|q| {
            tools.with(|t| {
                t.iter()
                    .filter(|tool| logic::tool_matches(tool, q))
                    .map(|tool| tool.name.clone())
                    .collect::<Vec<_>>()
            })
        })
    });
    let show = use_show_more(
        filtered.into(),
        TOOL_PAGE,
        Signal::derive(move || query.get()),
    );
    let filtering = Memo::new(move |_| query.with(|q| !q.trim().is_empty()));
    let visible_count = Memo::new(move |_| {
        tools.with(|t| hidden.with(|h| t.iter().filter(|tool| !h.contains(&tool.name)).count()))
    });
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);

    let bulk = move |visible: bool| {
        let names = filtered.get_untracked();
        let base = hidden.get_untracked();
        hidden_draft.set(Some(logic::set_visibility(&base, &names, visible)));
    };
    let toggle = Callback::new(move |(name, visible): (String, bool)| {
        let base = hidden.get_untracked();
        hidden_draft.set(Some(logic::set_visibility(&base, [&name], visible)));
    });
    let save = move |_| {
        let Some(next) = hidden_draft.get_untracked() else {
            return;
        };
        let Some(id) = gw
            .detail
            .with_untracked(|d| d.as_ref().map(|d| d.summary.id.clone()))
        else {
            return;
        };
        let body = tools.with_untracked(|t| logic::visibility_update(t, &next));
        saving.set(true);
        error.set(None);
        spawn_detached(async move {
            match api::set_gateway_tool_visibility(&id, &body).await {
                Ok(detail) => {
                    gw.toast.show(
                        format!(
                            "{}: serving {} of {} tools",
                            detail.summary.name,
                            detail.summary.visible_tool_count,
                            detail.summary.tool_count
                        ),
                        ToastKind::Info,
                    );
                    hidden_draft.try_set(None);
                    gw.apply(detail);
                }
                Err(e) => {
                    error.try_set(Some(e.message().to_owned()));
                }
            }
            saving.try_set(false);
        });
    };

    view! {
        <section class="gw-tools">
            {move || if tools.with(Vec::is_empty) {
                view! {
                    <EmptyState
                        compact=true
                        message="No tools yet. Refresh once the integration can connect."
                    />
                }
                .into_any()
            } else {
                view! {
                    <div class="toolbar gw-tools-toolbar">
                        <SearchField
                            value=query
                            on_input=Callback::new(move |q| query.set(q))
                            placeholder="Filter tools"
                            aria_label="Filter tools"
                        />
                        <span class="small muted num" role="status">
                            {move || format!("{} of {} served", visible_count.get(), tools.with(Vec::len))}
                        </span>
                        <span class="cluster gw-bulk">
                            <Button
                                size=ButtonSize::Sm
                                variant=ButtonVariant::Ghost
                                on_click=Callback::new(move |_| bulk(true))
                            >
                                {move || if filtering.get() { "Show matching" } else { "Show all" }}
                            </Button>
                            <Button
                                size=ButtonSize::Sm
                                variant=ButtonVariant::Ghost
                                on_click=Callback::new(move |_| bulk(false))
                            >
                                {move || if filtering.get() { "Hide matching" } else { "Hide all" }}
                            </Button>
                        </span>
                    </div>
                    {move || if filtered.with(Vec::is_empty) {
                        view! {
                            <EmptyState
                                compact=true
                                message=Signal::derive(move || format!("No tools match \u{201c}{}\u{201d}.", query.get()))
                                action=ViewFn::from(move || view! {
                                    <Button size=ButtonSize::Sm on_click=Callback::new(move |_| query.set(String::new()))>"Clear filter"</Button>
                                })
                            />
                        }
                        .into_any()
                    } else {
                        view! {
                            <div class="rows gw-tool-rows">
                                <For
                                    each=move || show.visible.get()
                                    key=|name| name.clone()
                                    children=move |name| view! { <ToolRow name tools hidden toggle/> }
                                />
                            </div>
                            {move || show.has_more.get().then(|| view! {
                                <ShowMoreButton
                                    remaining=show.remaining
                                    noun="tools"
                                    on_click=Callback::new(move |()| show.show_more())
                                />
                            })}
                        }
                        .into_any()
                    }}
                }
                .into_any()
            }}
            {move || (changes.get() > 0 || error.with(Option::is_some)).then(|| view! {
                <div class="gw-savebar" role="region" aria-label="Unsaved tool visibility">
                    <span class="small">
                        {move || logic::plural(changes.get(), "unsaved change", "unsaved changes")}
                    </span>
                    {move || error.get().map(|e| view! { <span class="field-error" role="alert">{e}</span> })}
                    <span class="spacer"></span>
                    <Button
                        size=ButtonSize::Sm
                        variant=ButtonVariant::Ghost
                        on_click=Callback::new(move |_| {
                            hidden_draft.set(None);
                            error.set(None);
                        })
                    >
                        "Discard"
                    </Button>
                    <Button
                        size=ButtonSize::Sm
                        variant=ButtonVariant::Primary
                        icon=Icon::Check
                        busy=saving
                        disabled=Signal::derive(move || changes.get() == 0)
                        disabled_reason="No change to save"
                        on_click=Callback::new(save)
                    >
                        "Save visibility"
                    </Button>
                </div>
            })}
        </section>
    }
}

fn annotation_tags(tool: &GatewayToolView) -> impl IntoView + use<> {
    let read_only = (tool.read_only == Some(true)).then(|| {
        view! { <Tag title="The integration marks this tool read-only">"read-only"</Tag> }
    });
    let destructive = (tool.destructive == Some(true)).then(|| {
        view! { <Tag tone=Tone::Fault title="May delete or overwrite data">"destructive"</Tag> }
    });
    let open_world = (tool.open_world == Some(true)).then(|| {
        view! { <Tag title="Reaches systems outside the integration (web, third parties)">"open-world"</Tag> }
    });
    view! { {read_only} {destructive} {open_world} }
}

#[component]
fn ToolRow(
    name: String,
    tools: Memo<Vec<GatewayToolView>>,
    hidden: Memo<BTreeSet<String>>,
    toggle: Callback<(String, bool)>,
) -> impl IntoView {
    let key = StoredValue::new(name);
    let tool = Memo::new(move |_| {
        tools.with(|t| key.with_value(|name| t.iter().find(|tool| tool.name == *name).cloned()))
    });
    let visible = Memo::new(move |_| hidden.with(|h| key.with_value(|name| !h.contains(name))));
    move || {
        let tool = tool.get()?;
        let title = tool.title.clone().unwrap_or_else(|| tool.name.clone());
        let change = tool.change.map(|change| match change {
            ToolChange::Added => {
                view! { <Tag tone=Tone::Signal title="Added by the last refresh">"new"</Tag> }
            }
            ToolChange::Changed => {
                view! { <Tag tone=Tone::Signal title="Changed by the last refresh">"changed"</Tag> }
            }
        });
        let exposed = tool.exposed_name.clone();
        let description = tool.description.clone();
        let tags = annotation_tags(&tool);
        let label = format!("Serve {title} to clients");
        Some(view! {
            <div class=move || if visible.get() { "row gw-tool" } else { "row gw-tool off" }>
                <span class="row-main">
                    <label class="gw-tool-head">
                        <input
                            type="checkbox"
                            aria-label=label
                            prop:checked=move || visible.get()
                            on:change=move |ev| toggle.run((key.get_value(), event_target_checked(&ev)))
                        />
                        <span class="row-title">{title}</span>
                        {change}
                    </label>
                    <span class="row-sub parts gw-exposed">
                        <span class="mono truncate" title=exposed.clone()>{exposed.clone()}</span>
                        <CopyButton text=Signal::stored(exposed.clone()) label=format!("Copy {exposed}")/>
                    </span>
                    {description.map(|d| {
                        let full = d.clone();
                        view! { <p class="small dim clamp-2" title=full>{d}</p> }
                    })}
                    <span class="cluster">{tags}</span>
                </span>
            </div>
        })
    }
}

// ----- resources and events -----

#[component]
fn ResourcesTab(gw: Gateway) -> impl IntoView {
    move || {
        gw.detail.with(|d| {
            let d = d.as_ref()?;
            let total = d.summary.resource_count as usize;
            if d.resources.is_empty() {
                return Some(
                    view! { <EmptyState compact=true message="This integration offers no resources."/> }
                        .into_any(),
                );
            }
            let note = (total > d.resources.len()).then(|| {
                view! {
                    <p class="small muted gw-note">{format!("Showing the first {} of {total}.", d.resources.len())}</p>
                }
            });
            let rows = d
                .resources
                .iter()
                .map(|r| {
                    let title = r.name.clone().unwrap_or_else(|| r.uri.clone());
                    view! {
                        <div class="row">
                            <span class="row-main">
                                <span class="row-title">{title}</span>
                                <span class="row-sub mono" title=r.uri.clone()>{r.uri.clone()}</span>
                            </span>
                            {r.mime_type.clone().map(|m| view! { <span class="row-end"><Tag>{m}</Tag></span> })}
                        </div>
                    }
                })
                .collect_view();
            Some(view! { {note} <div class="rows dense">{rows}</div> }.into_any())
        })
    }
}

#[component]
fn EventsTab(gw: Gateway) -> impl IntoView {
    move || {
        gw.detail.with(|d| {
            let d = d.as_ref()?;
            if d.events.is_empty() {
                return Some(
                    view! { <EmptyState compact=true message="This integration publishes no events."/> }
                        .into_any(),
                );
            }
            let rows = d
                .events
                .iter()
                .map(|e| {
                    let (kind, word) = if e.supported {
                        (StatusKind::Ok, "Offered")
                    } else {
                        (StatusKind::Idle, "Not offered")
                    };
                    view! {
                        <div class="row gw-event">
                            <span class="row-main">
                                <span class="row-title">{e.name.clone()}</span>
                                <span class="row-sub mono">{e.exposed_name.clone()}</span>
                                {e.description.clone().map(|text| view! { <p class="small dim clamp-2">{text}</p> })}
                                {e.note.clone().map(|note| view! { <p class="small muted">{note}</p> })}
                            </span>
                            <span class="row-end"><Status kind label=word/></span>
                        </div>
                    }
                })
                .collect_view();
            Some(
                view! {
                    <p class="small muted gw-note">"Offered events reach clients through Omni's events/poll; each one starts a feed when a client subscribes."</p>
                    <div class="rows">{rows}</div>
                }
                .into_any(),
            )
        })
    }
}
