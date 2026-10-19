//! The integrations list: one row per integration with its status, counts,
//! freshness and the problem (with its fix) when it is not serving.

use leptos::prelude::*;
use omni_api::gateway::{GatewayErrorView, IntegrationStatus, IntegrationSummary};
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, EmptyState, Icon, Panel, Status, Tag,
};
use omni_web_kit::utils::format::format_absolute;

use super::detail::DetailTab;
use super::logic::{self, Fix};
use super::{Gateway, RefreshingStatus};

#[component]
pub(super) fn IntegrationsPanel(gw: Gateway) -> impl IntoView {
    let ids = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| {
                    o.integrations
                        .iter()
                        .map(|i| i.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    view! {
        <Panel
            title="Integrations"
            class="gw-integrations"
            head_end=ViewFn::from(move || view! { <span class="num">{move || ids.with(Vec::len)}</span> })
        >
            {move || if ids.with(Vec::is_empty) {
                view! {
                    <EmptyState
                        icon=Icon::Plug
                        message="No integrations yet. Add a remote MCP server or a bundled one to serve its tools through Omni."
                        action=ViewFn::from(move || view! {
                            <Button icon=Icon::Plus on_click=Callback::new(move |_| gw.editor_open.set(true))>
                                "Add integration"
                            </Button>
                        })
                    />
                }
                .into_any()
            } else {
                view! {
                    <div class="rows gw-integration-rows" data-primary-rows="true">
                        <For each=move || ids.get() key=|id| id.clone() children=move |id| view! {
                            <IntegrationRow id gw/>
                        }/>
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}

/// The status shape and word, or a running "Refreshing" while a manual
/// refresh is in flight.
#[component]
pub(super) fn IntegrationStatusChip(
    gw: Gateway,
    #[prop(into)] id: String,
    #[prop(into)] status: Signal<IntegrationStatus>,
    #[prop(into)] refreshing: Signal<bool>,
) -> impl IntoView {
    let id = StoredValue::new(id);
    move || {
        if refreshing.get() {
            view! { <RefreshingStatus gw id=id.get_value()/> }.into_any()
        } else {
            let (kind, word) = logic::integration_status(status.get());
            view! { <Status kind label=word/> }.into_any()
        }
    }
}

/// The problem line: headline, message, hint and the matching fix.
#[component]
pub(super) fn IssueLine(gw: Gateway, summary: Signal<Option<IntegrationSummary>>) -> impl IntoView {
    move || {
        let summary = summary.get()?;
        let error = summary.error.clone().filter(|_| summary.enabled);
        let fix = logic::fix_for(&summary);
        if error.is_none() && fix.is_none() {
            return None;
        }
        let fault = summary.status == IntegrationStatus::Error;
        let (headline, message, hint) = match error {
            Some(GatewayErrorView {
                kind,
                message,
                hint,
                ..
            }) => (
                logic::error_kind_label(kind).to_owned(),
                Some(message),
                hint,
            ),
            None => ("Credentials needed".to_owned(), None, None),
        };
        let id = summary.id.clone();
        let action = fix.map(|fix| {
            let (label, icon) = match fix {
                Fix::Connect => ("Connect", Icon::External),
                Fix::Credentials => ("Set credentials", Icon::Pencil),
                Fix::ChooseAuth => ("Choose auth", Icon::Pencil),
            };
            let busy = Signal::derive({
                let id = id.clone();
                move || fix == Fix::Connect && gw.connecting.with(|c| c.as_deref() == Some(id.as_str()))
            });
            let id = id.clone();
            view! {
                <Button
                    size=ButtonSize::Sm
                    icon=icon
                    busy
                    aria_label=format!("{label} for {}", summary.name)
                    on_click=Callback::new(move |_| match fix {
                        Fix::Connect => gw.connect(id.clone()),
                        Fix::Credentials | Fix::ChooseAuth => gw.open(id.clone(), DetailTab::Settings),
                    })
                >
                    {label}
                </Button>
            }
        });
        Some(view! {
            <div class=if fault { "gw-issue fault" } else { "gw-issue warn" }>
                <div class="gw-issue-text">
                    <span class="gw-issue-head">{headline}</span>
                    {message.map(|m| view! { <span class="gw-issue-msg">{m}</span> })}
                    {hint.map(|h| view! { <span class="gw-issue-hint">{h}</span> })}
                </div>
                {action}
            </div>
        })
    }
}

#[component]
fn IntegrationRow(id: String, gw: Gateway) -> impl IntoView {
    let key = StoredValue::new(id.clone());
    let item = Memo::new(move |_| {
        gw.overview.with(|o| {
            key.with_value(|id| {
                o.as_ref()?
                    .integrations
                    .iter()
                    .find(|i| i.id == *id)
                    .cloned()
            })
        })
    });
    let refreshing = Signal::derive(move || key.with_value(|id| gw.is_refreshing(id)));
    let read = move |f: fn(&IntegrationSummary) -> String| {
        Signal::derive(move || item.with(|i| i.as_ref().map(f).unwrap_or_default()))
    };
    let name = read(|i| i.name.clone());
    let enabled = Memo::new(move |_| item.with(|i| i.as_ref().is_some_and(|i| i.enabled)));
    let status = Signal::derive(move || {
        item.with(|i| i.as_ref().map_or(IntegrationStatus::Pending, |i| i.status))
    });
    let toggling = Signal::derive(move || {
        gw.toggling
            .with(|t| key.with_value(|id| t.as_deref() == Some(id.as_str())))
    });
    let open = move |tab: DetailTab| gw.open(key.get_value(), tab);
    let facts = move || {
        item.with(|i| {
            let i = i.as_ref()?;
            let tools = if i.visible_tool_count == i.tool_count {
                logic::plural(i.tool_count as usize, "tool", "tools")
            } else {
                format!("{} of {} tools", i.visible_tool_count, i.tool_count)
            };
            let events = (i.event_count > 0)
                .then(|| view! { <span>{logic::plural(i.event_count as usize, "event", "events")}</span> });
            let refreshed = i.last_refresh_at.map(|at| at as f64);
            let (auth_label, auth_tag) = (logic::auth_label(i.auth), logic::auth_tag(i.auth));
            Some(view! {
                <Tag title=auth_label>{auth_tag}</Tag>
                <span class="dot-sep">
                    <span class="num">{tools}</span>
                    {events}
                    <span title=refreshed.map(format_absolute)>
                        {move || match refreshed {
                            Some(at) => format!("refreshed {}", logic::ago(at, gw.now.get())),
                            None => "never refreshed".to_owned(),
                        }}
                    </span>
                </span>
            })
        })
    };
    view! {
        <div
            class=move || if enabled.get() { "row gw-row" } else { "row gw-row off" }
            id=format!("integration-{id}")
        >
            <span class="gw-row-status">
                <IntegrationStatusChip gw id=id.clone() status refreshing/>
            </span>
            <span class="row-main">
                <button
                    type="button"
                    class="row-title gw-open"
                    data-row="true"
                    title=Signal::derive(move || format!("Open {}", name.get()))
                    on:click=move |_| open(DetailTab::Tools)
                >
                    {move || name.get()}
                </button>
                <span class="row-sub mono" title=read(|i| i.endpoint.clone())>
                    {move || item.with(|i| i.as_ref().map(|i| format!("{} · {}", i.id, i.endpoint)).unwrap_or_default())}
                </span>
                <span class="row-sub parts gw-facts">{facts}</span>
                <IssueLine gw summary=item.into()/>
            </span>
            <span class="row-end">
                <Button
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Sm
                    icon=Icon::Refresh
                    icon_only=true
                    busy=refreshing
                    disabled=Signal::derive(move || !enabled.get())
                    disabled_reason="Enable the integration to refresh it"
                    aria_label=Signal::derive(move || format!("Refresh {}", name.get()))
                    title=Signal::derive(move || format!("Refresh {}'s tools", name.get()))
                    on_click=Callback::new(move |_| gw.refresh(key.get_value()))
                />
                <span class="hide-phone">
                    <Button
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Sm
                        busy=toggling
                        aria_label=Signal::derive(move || format!("{} {}", if enabled.get() { "Disable" } else { "Enable" }, name.get()))
                        title=Signal::derive(move || if enabled.get() {
                            "Stop serving its tools and events".to_owned()
                        } else {
                            "Serve its tools and events again".to_owned()
                        })
                        on_click=Callback::new(move |_| gw.set_enabled(key.get_value(), !enabled.get_untracked()))
                    >
                        {move || if enabled.get() { "Disable" } else { "Enable" }}
                    </Button>
                </span>
                <Button
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Sm
                    icon=Icon::ChevronRight
                    icon_only=true
                    aria_label=Signal::derive(move || format!("Open {}", name.get()))
                    on_click=Callback::new(move |_| open(DetailTab::Tools))
                />
            </span>
        </div>
    }
}
