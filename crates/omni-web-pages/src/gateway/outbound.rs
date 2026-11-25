//! Outbound: where Omni's event journal delivers events and notices (stored
//! targets and the routes that feed them), and every recent delivery with
//! its attempts, so a dead delivery shows and can be sent again. Polls every
//! 30 s while the tab is visible, and again after every change.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use leptos::html::Input;
use leptos::prelude::*;
use omni_api::outbound::{
    AttemptView, DeliveryKind, DeliveryState, DeliveryView, EventRouteCreate, EventRoutePatch,
    EventRouteView, EventTargetUpsert, EventTargetView, OutboundOverview, RoutableEventView,
};
use omni_web_kit::api::{self, ApiClientError};
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, Chip, ConfirmButton, Disclosure, EmptyState, ErrorState,
    Icon, InlineNote, Panel, ShowMoreButton, SkeletonRows, Status, StatusKind, Tag, ToastKind,
    Tone, use_show_more,
};
use omni_web_kit::hooks::{scroll_into_view_center, use_page_visible};
use omni_web_kit::task::{sleep, spawn_detached, spawn_scoped};
use omni_web_kit::utils::format::{format_absolute, format_duration, format_relative_at};

use super::Gateway;
use super::logic::{self, MAX_LABEL_CHARS};

const POLL: Duration = Duration::from_secs(30);
/// Target ids are `^[a-z][a-z0-9-]{0,31}$` on the server.
const MAX_TARGET_ID_CHARS: usize = 32;
const DELIVERY_PAGE: usize = 25;
const DELIVERIES_ID: &str = "gateway-deliveries";

/// The section's state, shared by its panels.
#[derive(Clone, Copy)]
struct Outbound {
    overview: RwSignal<Option<OutboundOverview>>,
    failed_only: RwSignal<bool>,
    /// Which list the loaded deliveries are (failed only or all).
    shown_failed: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    nonce: RwSignal<u64>,
    gw: Gateway,
}

impl Outbound {
    fn reload(self) {
        self.nonce.try_update(|n| *n += 1);
    }

    fn edit(self, change: impl FnOnce(&mut OutboundOverview)) {
        self.overview.try_update(|o| {
            if let Some(o) = o {
                change(o);
            }
        });
    }

    fn refused(self, what: String, error: &ApiClientError) {
        self.gw
            .toast
            .show(format!("{what}: {}", error.message()), ToastKind::Error);
    }

    fn target_label_untracked(self, id: &str) -> String {
        self.overview
            .with_untracked(|o| {
                o.as_ref()?
                    .targets
                    .iter()
                    .find(|t| t.id == id)
                    .map(|t| t.label.clone())
            })
            .unwrap_or_else(|| id.to_owned())
    }
}

#[component]
pub(super) fn OutboundSection(gw: Gateway) -> impl IntoView {
    let ob = Outbound {
        overview: RwSignal::new(None),
        failed_only: RwSignal::new(false),
        shown_failed: RwSignal::new(false),
        error: RwSignal::new(None),
        nonce: RwSignal::new(0),
        gw,
    };

    // Switching the filter, a change or a returning tab fetches at once and
    // restarts the timer; a hidden tab stops polling.
    let visible = use_page_visible();
    Effect::new(move |_| {
        ob.nonce.track();
        let failed_only = ob.failed_only.get();
        if !visible.get() {
            return;
        }
        spawn_scoped(async move {
            loop {
                match api::fetch_outbound(failed_only).await {
                    Ok(next) => {
                        if ob.overview.with_untracked(|o| o.as_ref() != Some(&next)) {
                            ob.overview.set(Some(next));
                        }
                        if ob.shown_failed.get_untracked() != failed_only {
                            ob.shown_failed.set(failed_only);
                        }
                        if ob.error.with_untracked(Option::is_some) {
                            ob.error.set(None);
                        }
                    }
                    Err(e) => ob.error.set(Some(e.message().to_owned())),
                }
                sleep(POLL).await;
            }
        });
    });

    let loaded = Memo::new(move |_| ob.overview.with(Option::is_some));
    let error = Memo::new(move |_| ob.error.get());
    view! {
        <section class="gw-out" id="gateway-outbound" aria-labelledby="gw-out-title">
            <div class="section-head">
                <h2 class="section-title" id="gw-out-title">"Outbound"</h2>
                <span class="section-meta">"Where events and notices are delivered, and how each delivery went"</span>
            </div>
            {move || {
                if loaded.get() {
                    return view! {
                        <div class="stack">
                            {move || error.get().map(|e| view! {
                                <InlineNote tone=Tone::Warn role="status">
                                    {format!("Refresh failed ({e}). Showing the last loaded state.")}
                                </InlineNote>
                            })}
                            <Summary ob/>
                            <div class="grid-2 gw-out-config">
                                <TargetsPanel ob/>
                                <RoutesPanel ob/>
                            </div>
                            <DeliveriesPanel ob/>
                        </div>
                    }
                    .into_any();
                }
                match error.get() {
                    Some(e) => view! {
                        <Panel>
                            <ErrorState
                                title="Outbound deliveries could not load"
                                raw=e
                                retry=Callback::new(move |()| ob.reload())
                            />
                        </Panel>
                    }
                    .into_any(),
                    None => view! {
                        <Panel>
                            <SkeletonRows count=4 label="Loading outbound deliveries"/>
                        </Panel>
                    }
                    .into_any(),
                }
            }}
        </section>
    }
}

// ----- summary -----

#[component]
fn Summary(ob: Outbound) -> impl IntoView {
    let counts = Memo::new(move |_| {
        ob.overview
            .with(|o| o.as_ref().map(|o| o.counts.clone()).unwrap_or_default())
    });
    let failing = Memo::new(move |_| counts.with(|c| c.failed > 0));
    let pending = move || counts.with(|c| c.pending);
    let delivered = move || counts.with(|c| c.delivered);
    move || {
        if failing.get() {
            view! {
                <div class="gw-attention gw-out-summary" role="status">
                    <span class="status warn">
                        {move || format!(
                            "{} in the last 7 days",
                            logic::plural(counts.with(|c| c.failed) as usize, "delivery failed", "deliveries failed")
                        )}
                    </span>
                    <span class="small dim">
                        {move || format!("{} pending · {} delivered in 24 h", pending(), delivered())}
                    </span>
                    <Button
                        size=ButtonSize::Sm
                        on_click=Callback::new(move |_| {
                            ob.failed_only.set(true);
                            scroll_into_view_center(DELIVERIES_ID);
                        })
                    >
                        "Show failed"
                    </Button>
                </div>
            }
            .into_any()
        } else {
            view! {
                <p class="gw-out-summary small muted dot-sep">
                    <span><span class="num">{pending}</span>" pending"</span>
                    <span>"none failed in 7 days"</span>
                    <span><span class="num">{delivered}</span>" delivered in 24 h"</span>
                </p>
            }
            .into_any()
        }
    }
}

// ----- targets -----

/// Which target form is open.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TargetForm {
    New,
    Edit(String),
}

#[component]
fn TargetsPanel(ob: Outbound) -> impl IntoView {
    let ids = Memo::new(move |_| {
        ob.overview.with(|o| {
            o.as_ref()
                .map(|o| o.targets.iter().map(|t| t.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let empty = Memo::new(move |_| ids.with(Vec::is_empty));
    let form = RwSignal::new(None::<TargetForm>);
    let adding = Memo::new(move |_| form.with(|f| *f == Some(TargetForm::New)));
    let removing = RwSignal::new(None::<String>);
    let remove = Callback::new(move |id: String| {
        if removing.get_untracked().is_some() {
            return;
        }
        let label = ob.target_label_untracked(&id);
        removing.set(Some(id.clone()));
        spawn_detached(async move {
            match api::delete_outbound_target(&id).await {
                Ok(()) => {
                    ob.gw
                        .toast
                        .show(format!("Removed {label}"), ToastKind::Info);
                    ob.edit(|o| o.targets.retain(|t| t.id != id));
                }
                Err(e) => ob.refused(format!("Could not remove {label}"), &e),
            }
            removing.try_set(None);
            ob.reload();
        });
    });
    view! {
        <Panel
            title="Targets"
            class="gw-out-targets"
            head_end=ViewFn::from(move || view! {
                <span class="num">{move || ids.with(Vec::len)}</span>
                <Button
                    size=ButtonSize::Sm
                    icon=Icon::Plus
                    disabled=Signal::derive(move || form.with(Option::is_some))
                    disabled_reason="Finish the open target form first"
                    on_click=Callback::new(move |_| form.set(Some(TargetForm::New)))
                >
                    "Add target"
                </Button>
            })
        >
            {move || adding.get().then(|| view! { <TargetEditor ob form mode=TargetForm::New/> })}
            {move || if empty.get() {
                view! {
                    <EmptyState
                        compact=true
                        message="No targets yet. Add a bot webhook to deliver events and notices to."
                    />
                }
                .into_any()
            } else {
                view! {
                    <div class="rows">
                        <For
                            each=move || ids.get()
                            key=|id| id.clone()
                            children=move |id| view! { <TargetRow id ob form removing remove/> }
                        />
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}

#[component]
fn TargetRow(
    id: String,
    ob: Outbound,
    form: RwSignal<Option<TargetForm>>,
    removing: RwSignal<Option<String>>,
    remove: Callback<String>,
) -> impl IntoView {
    let key = StoredValue::new(id);
    let item = Memo::new(move |_| {
        ob.overview.with(|o| {
            key.with_value(|id| o.as_ref()?.targets.iter().find(|t| t.id == *id).cloned())
        })
    });
    let routes = Memo::new(move |_| {
        ob.overview.with(|o| {
            key.with_value(|id| {
                o.as_ref()
                    .map_or(0, |o| o.routes.iter().filter(|r| r.target == *id).count())
            })
        })
    });
    let editing = Memo::new(move |_| {
        form.with(|f| key.with_value(|id| f.as_ref() == Some(&TargetForm::Edit(id.clone()))))
    });
    let busy = Signal::derive(move || {
        removing.with(|r| key.with_value(|id| r.as_deref() == Some(id.as_str())))
    });
    let row = move || {
        let EventTargetView {
            id,
            label,
            url,
            key_set,
            connect,
            updated_at,
            ..
        } = item.get()?;
        let updated = updated_at as f64;
        let host = url_host(&url).to_owned();
        let used = routes.get();
        let remove_title = if used == 0 {
            "Stop delivering to it".to_owned()
        } else {
            format!(
                "{} deliver to it",
                logic::plural(used, "route still", "routes still")
            )
        };
        Some(view! {
            <div class="row gw-out-row">
                <span class="row-main">
                    <span class="row-title">
                        {label.clone()}
                        " "
                        <span class="mono small muted">{id}</span>
                    </span>
                    <span class="row-sub parts">
                        <span class="mono truncate" title=url>{host}</span>
                        {if key_set {
                            view! { <Tag title="Sent as a bearer token">"key set"</Tag> }.into_any()
                        } else {
                            view! { <Tag tone=Tone::Warn title="No bearer key is stored">"no key"</Tag> }.into_any()
                        }}
                    </span>
                    <span class="row-sub dot-sep">
                        {connect.map(|host| view! {
                            <span class="mono" title="Connects here instead of the URL's host; TLS still checks the URL's host">
                                {format!("via {host}")}
                            </span>
                        })}
                        <span title=format_absolute(updated)>
                            {move || format!("updated {}", logic::ago(updated, ob.gw.now.get()))}
                        </span>
                    </span>
                </span>
                <span class="row-end">
                    <Button
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Sm
                        icon=Icon::Pencil
                        icon_only=true
                        aria_label=format!("Edit {label}")
                        title=format!("Edit {label}")
                        disabled=Signal::derive(move || form.with(Option::is_some))
                        disabled_reason="Finish the open target form first"
                        on_click=Callback::new(move |_| form.set(Some(TargetForm::Edit(key.get_value()))))
                    />
                    <ConfirmButton
                        label="Remove"
                        confirm_label="Remove target"
                        context=format!("target {label}")
                        destructive=true
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Sm
                        busy
                        title=remove_title
                        on_confirm=Callback::new(move |()| remove.run(key.get_value()))
                    />
                </span>
            </div>
        })
    };
    view! {
        {row}
        {move || editing.get().then(|| view! {
            <TargetEditor ob form mode=TargetForm::Edit(key.get_value())/>
        })}
    }
}

/// A labelled text input with its help and error lines.
#[component]
fn TextField(
    id: &'static str,
    label: &'static str,
    #[prop(into)] value: Signal<String>,
    on_input: Callback<String>,
    #[prop(into)] error: Signal<Option<String>>,
    #[prop(into, optional)] help: MaybeProp<String>,
    #[prop(optional)] input_type: Option<&'static str>,
    #[prop(optional)] autocomplete: Option<&'static str>,
    #[prop(optional)] placeholder: &'static str,
    #[prop(optional)] maxlength: Option<usize>,
    #[prop(optional)] mono: bool,
    #[prop(optional)] node_ref: Option<NodeRef<Input>>,
) -> impl IntoView {
    let node_ref = node_ref.unwrap_or_default();
    view! {
        <div class="field">
            <label class="field-label" for=id>{label}</label>
            <input
                id=id
                node_ref=node_ref
                class=if mono { "input mono" } else { "input" }
                type=input_type.unwrap_or("text")
                autocomplete=autocomplete.unwrap_or("off")
                autocapitalize=mono.then_some("none")
                spellcheck="false"
                maxlength=maxlength.map(|n| n.to_string())
                placeholder=placeholder
                aria-describedby=format!("{id}-help {id}-error")
                aria-invalid=move || error.with(Option::is_some).then_some("true")
                prop:value=move || value.get()
                on:input=move |ev| on_input.run(event_target_value(&ev))
            />
            {move || help.get().map(|h| view! { <p class="field-help" id=format!("{id}-help")>{h}</p> })}
            {move || error.get().map(|e| view! { <p class="field-error" id=format!("{id}-error")>{e}</p> })}
        </div>
    }
}

#[component]
fn TargetEditor(
    ob: Outbound,
    form: RwSignal<Option<TargetForm>>,
    mode: TargetForm,
) -> impl IntoView {
    let (creating, current) = match &mode {
        TargetForm::New => (true, None),
        TargetForm::Edit(id) => (
            false,
            ob.overview
                .with_untracked(|o| o.as_ref()?.targets.iter().find(|t| t.id == *id).cloned()),
        ),
    };
    let key_set = current.as_ref().is_some_and(|t| t.key_set);
    let draft = RwSignal::new(current.as_ref().map(TargetDraft::of).unwrap_or_default());
    let existing = StoredValue::new(ob.overview.with_untracked(|o| {
        o.as_ref()
            .map(|o| o.targets.iter().map(|t| t.id.clone()).collect::<Vec<_>>())
            .unwrap_or_default()
    }));
    let problems = Memo::new(move |_| {
        draft.with(|d| existing.with_value(|ids| target_problems(d, creating, ids)))
    });
    let attempted = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let edited_id = match &mode {
        TargetForm::Edit(id) => id.clone(),
        TargetForm::New => String::new(),
    };
    let title = if creating {
        "New target".to_owned()
    } else {
        format!("Edit target {edited_id}")
    };
    let edited_id = StoredValue::new(edited_id);

    let id_ref = NodeRef::<Input>::new();
    let label_ref = NodeRef::<Input>::new();
    let first = if creating { id_ref } else { label_ref };
    Effect::new(move |_| {
        if let Some(input) = first.get() {
            let _ = input.focus();
        }
    });

    let text = move |pick: fn(&TargetDraft) -> String| Signal::derive(move || draft.with(pick));
    let set = move |apply: fn(&mut TargetDraft, String)| {
        Callback::new(move |value: String| draft.update(|d| apply(d, value)))
    };
    let shown = move |pick: fn(&TargetProblems) -> Option<String>| {
        Signal::derive(move || attempted.get().then(|| problems.with(pick)).flatten())
    };
    let key_help = if creating {
        "Sent as Authorization: Bearer <key>. Omni cannot show it again."
    } else if key_set {
        "Leave empty to keep the stored key."
    } else {
        "No key is stored. Leave empty to keep it that way."
    };

    let save = move || {
        if saving.get_untracked() {
            return;
        }
        attempted.set(true);
        if problems.with_untracked(TargetProblems::any) {
            return;
        }
        let (id, body) = draft.with_untracked(|d| {
            let id = if creating {
                d.id.trim().to_owned()
            } else {
                edited_id.get_value()
            };
            (id, d.upsert())
        });
        saving.set(true);
        spawn_detached(async move {
            match api::put_outbound_target(&id, &body).await {
                Ok(target) => {
                    let verb = if creating { "Added" } else { "Saved" };
                    ob.gw
                        .toast
                        .show(format!("{verb} {}", target.label), ToastKind::Info);
                    ob.edit(|o| match o.targets.iter_mut().find(|t| t.id == target.id) {
                        Some(slot) => *slot = target,
                        None => o.targets.push(target),
                    });
                    form.try_set(None);
                    ob.reload();
                }
                Err(e) => ob.refused(format!("Could not save {}", body.label), &e),
            }
            saving.try_set(false);
        });
    };

    view! {
        <form
            class="panel-body gw-out-form"
            aria-label=title
            novalidate=true
            on:submit=move |event| {
                event.prevent_default();
                save();
            }
        >
            {creating.then(|| view! {
                <TextField
                    id="gw-out-target-id"
                    label="Id"
                    value=text(|d| d.id.clone())
                    on_input=set(|d, v| d.id = v)
                    error=shown(|p| p.id.clone())
                    help="What routes and notices name: lowercase letters, digits and -. It cannot change later."
                    placeholder="kitchen"
                    maxlength=MAX_TARGET_ID_CHARS
                    mono=true
                    node_ref=id_ref
                />
            })}
            <TextField
                id="gw-out-target-label"
                label="Label"
                value=text(|d| d.label.clone())
                on_input=set(|d, v| d.label = v)
                error=shown(|p| p.label.clone())
                placeholder="Kitchen bot"
                maxlength=MAX_LABEL_CHARS
                node_ref=label_ref
            />
            <TextField
                id="gw-out-target-url"
                label="URL"
                value=text(|d| d.url.clone())
                on_input=set(|d, v| d.url = v)
                error=shown(|p| p.url.clone())
                help="The receiver's webhook. Omni posts JSON with an Idempotency-Key."
                input_type="url"
                placeholder="https://bots.example.com/webhook"
                mono=true
            />
            <TextField
                id="gw-out-target-key"
                label="Key"
                value=text(|d| d.key.clone())
                on_input=set(|d, v| d.key = v)
                error=shown(|p| p.key.clone())
                help=key_help
                input_type="password"
                autocomplete="new-password"
                mono=true
            />
            <TextField
                id="gw-out-target-connect"
                label="Connect via (optional)"
                value=text(|d| d.connect.clone())
                on_input=set(|d, v| d.connect = v)
                error=shown(|p| p.connect.clone())
                help="A host to connect to instead of the URL's, such as a proxy container. TLS still checks the URL's host."
                placeholder="proxy"
                mono=true
            />
            <div class="cluster">
                <Button submit=true busy=saving>{if creating { "Add target" } else { "Save target" }}</Button>
                <Button variant=ButtonVariant::Ghost on_click=Callback::new(move |_| form.set(None))>
                    "Cancel"
                </Button>
            </div>
        </form>
    }
}

// ----- routes -----

#[component]
fn RoutesPanel(ob: Outbound) -> impl IntoView {
    let ids = Memo::new(move |_| {
        ob.overview.with(|o| {
            o.as_ref()
                .map(|o| o.routes.iter().map(|r| r.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let empty = Memo::new(move |_| ids.with(Vec::is_empty));
    let has_targets = Memo::new(move |_| {
        ob.overview
            .with(|o| o.as_ref().is_some_and(|o| !o.targets.is_empty()))
    });
    let has_events = Memo::new(move |_| {
        ob.overview
            .with(|o| o.as_ref().is_some_and(|o| !o.events.is_empty()))
    });
    let adding = RwSignal::new(false);
    let busy = RwSignal::new(None::<String>);

    let toggle = Callback::new(move |(id, enabled): (String, bool)| {
        if busy.get_untracked().is_some() {
            return;
        }
        busy.set(Some(id.clone()));
        let body = EventRoutePatch {
            enabled: Some(enabled),
        };
        spawn_detached(async move {
            match api::update_outbound_route(&id, &body).await {
                Ok(route) => {
                    let verb = if route.enabled { "Enabled" } else { "Disabled" };
                    ob.gw
                        .toast
                        .show(format!("{verb} the {} route", route.event), ToastKind::Info);
                    ob.edit(|o| {
                        if let Some(slot) = o.routes.iter_mut().find(|r| r.id == route.id) {
                            *slot = route;
                        }
                    });
                }
                Err(e) => {
                    let verb = if enabled { "enable" } else { "disable" };
                    ob.refused(format!("Could not {verb} the route"), &e);
                }
            }
            busy.try_set(None);
            ob.reload();
        });
    });
    let remove = Callback::new(move |id: String| {
        if busy.get_untracked().is_some() {
            return;
        }
        busy.set(Some(id.clone()));
        spawn_detached(async move {
            match api::delete_outbound_route(&id).await {
                Ok(()) => {
                    ob.gw.toast.show("Removed the route", ToastKind::Info);
                    ob.edit(|o| o.routes.retain(|r| r.id != id));
                }
                Err(e) => ob.refused("Could not remove the route".to_owned(), &e),
            }
            busy.try_set(None);
            ob.reload();
        });
    });

    view! {
        <Panel
            title="Routes"
            class="gw-out-routes"
            head_end=ViewFn::from(move || view! {
                <span class="num">{move || ids.with(Vec::len)}</span>
                <Button
                    size=ButtonSize::Sm
                    icon=Icon::Plus
                    disabled=Signal::derive(move || adding.get() || !has_targets.get() || !has_events.get())
                    disabled_reason=Signal::derive(move || {
                        if adding.get() {
                            "Finish the route below first"
                        } else if !has_targets.get() {
                            "Add a target first"
                        } else {
                            "No event can be routed"
                        }
                        .to_owned()
                    })
                    on_click=Callback::new(move |_| adding.set(true))
                >
                    "Add route"
                </Button>
            })
        >
            {move || adding.get().then(|| view! { <RouteEditor ob open=adding/> })}
            {move || if empty.get() {
                view! {
                    <EmptyState
                        compact=true
                        message="No routes. A route delivers every matching event to a target."
                    />
                }
                .into_any()
            } else {
                view! {
                    <div class="rows">
                        <For
                            each=move || ids.get()
                            key=|id| id.clone()
                            children=move |id| view! { <RouteRow id ob busy toggle remove/> }
                        />
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}

#[component]
fn RouteRow(
    id: String,
    ob: Outbound,
    busy: RwSignal<Option<String>>,
    toggle: Callback<(String, bool)>,
    remove: Callback<String>,
) -> impl IntoView {
    let key = StoredValue::new(id);
    let item = Memo::new(move |_| {
        ob.overview
            .with(|o| key.with_value(|id| o.as_ref()?.routes.iter().find(|r| r.id == *id).cloned()))
    });
    // Event, arguments and target never change on a route; only `enabled` does.
    let Some(EventRouteView {
        event,
        arguments,
        target,
        created_at,
        ..
    }) = item.get_untracked()
    else {
        return ().into_any();
    };
    let enabled = Memo::new(move |_| item.with(|r| r.as_ref().is_some_and(|r| r.enabled)));
    let target_label = {
        let target = target.clone();
        Memo::new(move |_| {
            ob.overview.with(|o| {
                o.as_ref()?
                    .targets
                    .iter()
                    .find(|t| t.id == target)
                    .map(|t| t.label.clone())
            })
        })
    };
    let line = {
        let target = target.clone();
        Memo::new(move |_| {
            let name = target_label.get().unwrap_or_else(|| target.clone());
            route_line(&event, &arguments, &name)
        })
    };
    let is_busy = Signal::derive(move || {
        busy.with(|b| key.with_value(|id| b.as_deref() == Some(id.as_str())))
    });
    let created = created_at as f64;
    view! {
        <div class=move || if enabled.get() { "row gw-out-row" } else { "row gw-out-row off" }>
            <span class="row-main">
                <span class="row-title mono gw-out-route" title=move || line.get()>{move || line.get()}</span>
                <span class="row-sub dot-sep">
                    {move || (!enabled.get()).then(|| view! { <span>"disabled"</span> })}
                    {move || target_label.with(Option::is_none).then(|| view! {
                        <span class="text-warn">{format!("no target named {target}")}</span>
                    })}
                    <span title=format_absolute(created)>
                        {move || format!("added {}", logic::ago(created, ob.gw.now.get()))}
                    </span>
                </span>
            </span>
            <span class="row-end">
                <Button
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Sm
                    busy=is_busy
                    aria_label=Signal::derive(move || format!(
                        "{} route {}",
                        if enabled.get() { "Disable" } else { "Enable" },
                        line.get()
                    ))
                    title=Signal::derive(move || if enabled.get() {
                        "Stop delivering these events".to_owned()
                    } else {
                        "Deliver these events again".to_owned()
                    })
                    on_click=Callback::new(move |_| toggle.run((key.get_value(), !enabled.get_untracked())))
                >
                    {move || if enabled.get() { "Disable" } else { "Enable" }}
                </Button>
                <ConfirmButton
                    label="Remove"
                    confirm_label="Remove route"
                    context=Signal::derive(move || format!("route {}", line.get()))
                    destructive=true
                    variant=ButtonVariant::Ghost
                    size=ButtonSize::Sm
                    busy=is_busy
                    title="Stop delivering these events and forget the route"
                    on_confirm=Callback::new(move |()| remove.run(key.get_value()))
                />
            </span>
        </div>
    }
    .into_any()
}

#[component]
fn RouteEditor(ob: Outbound, open: RwSignal<bool>) -> impl IntoView {
    let events = Memo::new(move |_| {
        ob.overview
            .with(|o| o.as_ref().map(|o| o.events.clone()).unwrap_or_default())
    });
    let targets = Memo::new(move |_| {
        ob.overview.with(|o| {
            o.as_ref()
                .map(|o| {
                    o.targets
                        .iter()
                        .map(|t| (t.id.clone(), t.label.clone()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let event = RwSignal::new(String::new());
    let values = RwSignal::new(BTreeMap::<String, String>::new());
    let target = RwSignal::new(targets.with_untracked(|t| match t.as_slice() {
        [(only, _)] => only.clone(),
        _ => String::new(),
    }));
    let chosen = Memo::new(move |_| {
        event.with(|name| events.with(|list| list.iter().find(|e| e.name == *name).cloned()))
    });
    let problem = Memo::new(move |_| {
        event.with(|e| {
            target.with(|t| {
                ob.overview
                    .with(|o| o.as_ref().and_then(|o| route_problem(e, t, o)))
            })
        })
    });
    let attempted = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let event_ref = NodeRef::<leptos::html::Select>::new();
    Effect::new(move |_| {
        if let Some(select) = event_ref.get() {
            let _ = select.focus();
        }
    });

    let save = move || {
        if saving.get_untracked() {
            return;
        }
        attempted.set(true);
        if problem.with_untracked(Option::is_some) {
            return;
        }
        let Some(chosen) = chosen.get_untracked() else {
            return;
        };
        let body = values.with_untracked(|v| route_create(&chosen, v, &target.get_untracked()));
        let target_name = ob.target_label_untracked(&body.target);
        saving.set(true);
        spawn_detached(async move {
            match api::create_outbound_route(&body).await {
                Ok(route) => {
                    ob.gw.toast.show(
                        format!(
                            "Added {}",
                            route_line(&route.event, &route.arguments, &target_name)
                        ),
                        ToastKind::Info,
                    );
                    ob.edit(|o| {
                        if !o.routes.iter().any(|r| r.id == route.id) {
                            o.routes.push(route);
                        }
                    });
                    open.try_set(false);
                    ob.reload();
                }
                Err(e) => ob.refused("Could not add the route".to_owned(), &e),
            }
            saving.try_set(false);
        });
    };

    let arguments = move || {
        chosen.get().map(|e| {
            if e.arguments.is_empty() {
                return view! {
                    <p class="field-help">{format!("Every {} event goes to the target.", e.name)}</p>
                }
                .into_any();
            }
            let inputs = e
                .arguments
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let input_id = format!("gw-out-route-arg-{i}");
                    let read = name.clone();
                    let write = name.clone();
                    view! {
                        <div class="field">
                            <label class="field-label mono" for=input_id.clone()>{name.clone()}</label>
                            <input
                                id=input_id
                                class="input mono"
                                type="text"
                                autocomplete="off"
                                autocapitalize="none"
                                spellcheck="false"
                                placeholder="any"
                                prop:value=move || values.with(|v| v.get(&read).cloned().unwrap_or_default())
                                on:input=move |ev| {
                                    let value = event_target_value(&ev);
                                    values.update(|v| {
                                        v.insert(write.clone(), value);
                                    });
                                }
                            />
                        </div>
                    }
                })
                .collect_view();
            view! {
                <fieldset class="gw-out-args">
                    <legend class="field-label">"Only when"</legend>
                    <div class="gw-out-arg-grid">{inputs}</div>
                    <p class="field-help">"Leave an argument empty to match any value."</p>
                </fieldset>
            }
            .into_any()
        })
    };

    view! {
        <form
            class="panel-body gw-out-form"
            aria-label="New route"
            novalidate=true
            on:submit=move |ev| {
                ev.prevent_default();
                save();
            }
        >
            <div class="field">
                <label class="field-label" for="gw-out-route-event">"Event"</label>
                <select
                    id="gw-out-route-event"
                    class="select"
                    node_ref=event_ref
                    aria-describedby="gw-out-route-event-help"
                    on:change=move |ev| {
                        event.set(event_target_value(&ev));
                        values.set(BTreeMap::new());
                    }
                >
                    <option value="" disabled=true prop:selected=move || event.with(String::is_empty)>
                        "Choose an event"
                    </option>
                    {move || events.get().into_iter().map(|e| {
                        let name = e.name.clone();
                        view! {
                            <option value=e.name.clone() prop:selected=move || event.with(|v| *v == name)>
                                {e.name.clone()}
                            </option>
                        }
                    }).collect_view()}
                </select>
                {move || chosen.get().map(|e| view! {
                    <p class="field-help" id="gw-out-route-event-help">{e.description}</p>
                })}
            </div>
            {arguments}
            <div class="field">
                <label class="field-label" for="gw-out-route-target">"Target"</label>
                <select
                    id="gw-out-route-target"
                    class="select"
                    on:change=move |ev| target.set(event_target_value(&ev))
                >
                    <option value="" disabled=true prop:selected=move || target.with(String::is_empty)>
                        "Choose a target"
                    </option>
                    {move || targets.get().into_iter().map(|(id, label)| {
                        let mine = id.clone();
                        view! {
                            <option value=id.clone() prop:selected=move || target.with(|t| *t == mine)>
                                {format!("{label} ({id})")}
                            </option>
                        }
                    }).collect_view()}
                </select>
            </div>
            {move || attempted.get().then(|| problem.get()).flatten().map(|e| view! {
                <p class="field-error" role="alert">{e}</p>
            })}
            <div class="cluster">
                <Button submit=true busy=saving>"Add route"</Button>
                <Button variant=ButtonVariant::Ghost on_click=Callback::new(move |_| open.set(false))>
                    "Cancel"
                </Button>
            </div>
        </form>
    }
}

// ----- deliveries -----

#[component]
fn DeliveriesPanel(ob: Outbound) -> impl IntoView {
    let ids = Memo::new(move |_| {
        ob.overview.with(|o| {
            o.as_ref()
                .map(|o| {
                    o.deliveries
                        .iter()
                        .map(|d| d.id.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });
    let empty = Memo::new(move |_| ids.with(Vec::is_empty));
    let failed = Memo::new(move |_| {
        ob.overview
            .with(|o| o.as_ref().map_or(0, |o| o.counts.failed as usize))
    });
    let labels = Memo::new(move |_| {
        ob.overview.with(|o| {
            o.as_ref()
                .map(|o| {
                    o.targets
                        .iter()
                        .map(|t| (t.id.clone(), t.label.clone()))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default()
        })
    });
    let pages = use_show_more(
        ids.into(),
        DELIVERY_PAGE,
        Signal::derive(move || ob.shown_failed.get().to_string()),
    );
    // The other list is on its way: dim this one instead of clearing it.
    let switching = Signal::derive(move || ob.shown_failed.get() != ob.failed_only.get());
    let retrying = RwSignal::new(None::<String>);
    let retry = Callback::new(move |id: String| {
        if retrying.get_untracked().is_some() {
            return;
        }
        retrying.set(Some(id.clone()));
        spawn_detached(async move {
            match api::retry_outbound_delivery(&id).await {
                Ok(delivery) => {
                    let name = labels.with_untracked(|l| {
                        destination_name(delivery.kind, &delivery.destination, l)
                    });
                    let (message, ok) = retry_outcome(&delivery, &name);
                    let kind = if ok {
                        ToastKind::Info
                    } else {
                        ToastKind::Error
                    };
                    ob.gw.toast.show(message, kind);
                    ob.edit(|o| {
                        if let Some(slot) = o.deliveries.iter_mut().find(|d| d.id == delivery.id) {
                            *slot = delivery;
                        }
                    });
                }
                Err(e) => ob.refused("Could not retry the delivery".to_owned(), &e),
            }
            retrying.try_set(None);
            ob.reload();
        });
    });

    view! {
        <Panel
            title="Deliveries"
            id=DELIVERIES_ID
            class="gw-out-deliveries"
            refreshing=switching
            head_end=ViewFn::from(move || view! {
                <span class="hide-phone">"Newest first"</span>
                <Chip
                    pressed=ob.failed_only
                    count=Signal::derive(move || Some(failed.get()))
                    title="Only deliveries that failed for good"
                    on_click=Callback::new(move |()| ob.failed_only.update(|f| *f = !*f))
                >
                    "Only failed"
                </Chip>
            })
        >
            {move || if empty.get() {
                if ob.shown_failed.get() {
                    view! {
                        <EmptyState
                            compact=true
                            message="No failed deliveries."
                            action=ViewFn::from(move || view! {
                                <Button
                                    size=ButtonSize::Sm
                                    variant=ButtonVariant::Ghost
                                    on_click=Callback::new(move |_| ob.failed_only.set(false))
                                >
                                    "Show all"
                                </Button>
                            })
                        />
                    }
                    .into_any()
                } else {
                    view! {
                        <EmptyState
                            compact=true
                            message="No deliveries yet. They appear here once a route, notice or subscription sends an event."
                        />
                    }
                    .into_any()
                }
            } else {
                view! {
                    <div class="rows">
                        <For
                            each=move || pages.visible.get()
                            key=|id| id.clone()
                            children=move |id| view! { <DeliveryRow id ob labels retrying retry/> }
                        />
                    </div>
                    {move || pages.has_more.get().then(|| view! {
                        <div class="panel-foot gw-out-more">
                            <ShowMoreButton
                                remaining=pages.remaining
                                noun="deliveries"
                                on_click=Callback::new(move |()| pages.show_more())
                            />
                        </div>
                    })}
                }
                .into_any()
            }}
        </Panel>
    }
}

/// One delivery. The row stays in place across polls (only its text
/// changes), so an open attempt history stays open.
#[component]
fn DeliveryRow(
    id: String,
    ob: Outbound,
    labels: Memo<HashMap<String, String>>,
    retrying: RwSignal<Option<String>>,
    retry: Callback<String>,
) -> impl IntoView {
    let key = StoredValue::new(id);
    let item = Memo::new(move |_| {
        ob.overview.with(|o| {
            key.with_value(|id| o.as_ref()?.deliveries.iter().find(|d| d.id == *id).cloned())
        })
    });
    let Some(DeliveryView {
        event,
        kind,
        destination,
        created_at,
        ..
    }) = item.get_untracked()
    else {
        return ().into_any();
    };
    let name = {
        let destination = destination.clone();
        Memo::new(move |_| labels.with(|l| destination_name(kind, &destination, l)))
    };
    let destination_title = match kind {
        DeliveryKind::Subscription => "An MCP client's webhook".to_owned(),
        DeliveryKind::Route | DeliveryKind::Notice => format!("Target {destination}"),
    };
    let status =
        Memo::new(move |_| item.with(|d| d.as_ref().map(|d| delivery_status(d.state, d.attempts))));
    let is_failed = Memo::new(move |_| {
        item.with(|d| d.as_ref().is_some_and(|d| d.state == DeliveryState::Failed))
    });
    let attempts = Memo::new(move |_| item.with(|d| d.as_ref().map_or(0, |d| d.attempts)));
    let next = Memo::new(move |_| {
        item.with(|d| {
            d.as_ref()
                .filter(|d| d.state == DeliveryState::Pending)
                .and_then(|d| d.next_attempt_at)
        })
    });
    let note = Memo::new(move |_| item.with(|d| d.as_ref().and_then(delivery_note)));
    let history = Memo::new(move |_| {
        item.with(|d| d.as_ref().map(|d| d.history.clone()).unwrap_or_default())
    });
    let has_history = Memo::new(move |_| history.with(|h| !h.is_empty()));
    let busy = Signal::derive(move || {
        retrying.with(|r| key.with_value(|id| r.as_deref() == Some(id.as_str())))
    });
    let created = created_at as f64;
    let retry_label = format!("Retry {event} to {}", name.get_untracked());
    view! {
        <div class="row gw-out-row">
            <span class="gw-out-state">
                {move || status.get().map(|(shape, word, dot_only)| view! { <Status kind=shape label=word dot_only/> })}
            </span>
            <span class="row-main">
                <span class="row-title mono" title=event.clone()>{event.clone()}</span>
                <span class="row-sub parts">
                    <Tag>{kind_label(kind)}</Tag>
                    <span class="dot-sep">
                        <span class="mono" title=destination_title>{move || name.get()}</span>
                        <span>{move || attempts_label(attempts.get())}</span>
                        <span title=format_absolute(created)>
                            {move || format!("queued {}", logic::ago(created, ob.gw.now.get()))}
                        </span>
                        {move || next.get().map(|at| {
                            let at = at as f64;
                            view! {
                                <span title=format_absolute(at)>
                                    {move || format!("next try {}", format_relative_at(at, ob.gw.now.get()))}
                                </span>
                            }
                        })}
                    </span>
                </span>
                {move || note.get().map(|(text, tone)| view! {
                    <span class=format!("row-sub gw-out-note {}", note_class(tone))>{text}</span>
                })}
                {move || has_history.get().then(|| view! {
                    <Disclosure
                        summary=Signal::derive(move || history.with(|h| logic::plural(h.len(), "attempt", "attempts")))
                        meta=Signal::derive(move || {
                            let shown = history.with(Vec::len);
                            let total = attempts.get() as usize;
                            (total > shown).then(|| format!("newest {shown} of {total}"))
                        })
                        flush=true
                        class="gw-out-history"
                    >
                        <ol class="gw-out-attempts" aria-label="Attempts, newest first">
                            {move || history.get().into_iter().map(attempt_item).collect_view()}
                        </ol>
                    </Disclosure>
                })}
            </span>
            <span class="row-end">
                {move || is_failed.get().then(|| view! {
                    <Button
                        size=ButtonSize::Sm
                        icon=Icon::Refresh
                        busy
                        aria_label=retry_label.clone()
                        title="Send it again; the receiver drops a repeat by its Idempotency-Key"
                        on_click=Callback::new(move |_| retry.run(key.get_value()))
                    >
                        "Retry"
                    </Button>
                })}
            </span>
        </div>
    }
    .into_any()
}

fn attempt_item(attempt: AttemptView) -> impl IntoView {
    let at = attempt.at as f64;
    let class = if attempt_failed(&attempt) {
        "text-fault"
    } else {
        "dim"
    };
    view! {
        <li>
            <span class="num">{format_absolute(at)}</span>
            <span class=class>{attempt_outcome(&attempt)}</span>
            <span class="num">{format_duration(f64::from(attempt.duration_ms))}</span>
        </li>
    }
}

fn note_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Fault => "text-fault",
        Tone::Warn => "text-warn",
        _ => "",
    }
}

// ----- pure decisions -----

/// What a target form holds. The key is never prefilled.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TargetDraft {
    id: String,
    label: String,
    url: String,
    key: String,
    connect: String,
}

impl TargetDraft {
    fn of(target: &EventTargetView) -> Self {
        Self {
            id: target.id.clone(),
            label: target.label.clone(),
            url: target.url.clone(),
            key: String::new(),
            connect: target.connect.clone().unwrap_or_default(),
        }
    }

    /// The PUT body: an empty key keeps the stored one, an empty connect
    /// host clears it (the PUT replaces everything but the key).
    fn upsert(&self) -> EventTargetUpsert {
        let optional = |value: &str| {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        };
        EventTargetUpsert {
            label: self.label.trim().to_owned(),
            url: self.url.trim().to_owned(),
            key: optional(&self.key),
            connect: optional(&self.connect),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TargetProblems {
    id: Option<String>,
    label: Option<String>,
    url: Option<String>,
    key: Option<String>,
    connect: Option<String>,
}

impl TargetProblems {
    fn any(&self) -> bool {
        [&self.id, &self.label, &self.url, &self.key, &self.connect]
            .iter()
            .any(|p| p.is_some())
    }
}

/// The server's rules, checked before the round trip. `existing` holds the
/// ids in use (a PUT to one of them would replace that target).
fn target_problems(draft: &TargetDraft, creating: bool, existing: &[String]) -> TargetProblems {
    TargetProblems {
        id: creating
            .then(|| target_id_problem(draft.id.trim(), existing))
            .flatten(),
        label: target_label_problem(&draft.label),
        url: target_url_problem(&draft.url),
        key: (creating && draft.key.trim().is_empty())
            .then(|| "Enter the key the receiver expects".to_owned()),
        connect: connect_problem(draft.connect.trim()),
    }
}

fn target_id_problem(id: &str, existing: &[String]) -> Option<String> {
    let Some(first) = id.chars().next() else {
        return Some("Enter an id, such as kitchen".to_owned());
    };
    if !first.is_ascii_lowercase() {
        return Some("Start with a lowercase letter".to_owned());
    }
    if id.chars().count() > MAX_TARGET_ID_CHARS {
        return Some(format!("At most {MAX_TARGET_ID_CHARS} characters"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Some("Use lowercase letters, digits and - only".to_owned());
    }
    if existing.iter().any(|e| e == id) {
        return Some("A target with this id exists; edit it instead".to_owned());
    }
    None
}

fn target_label_problem(label: &str) -> Option<String> {
    let label = label.trim();
    if label.is_empty() {
        Some("Name the receiver, such as \u{201c}Kitchen bot\u{201d}".to_owned())
    } else if label.chars().count() > MAX_LABEL_CHARS {
        Some(format!("At most {MAX_LABEL_CHARS} characters"))
    } else {
        None
    }
}

/// The length of a leading `https://` or `http://`, any case.
fn web_scheme_len(url: &str) -> Option<usize> {
    ["https://", "http://"]
        .into_iter()
        .find(|scheme| {
            url.get(..scheme.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
        })
        .map(str::len)
}

/// Why `url` cannot receive deliveries: http or https with a host, and no
/// credentials or fragment.
fn target_url_problem(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return Some("Enter the receiver's URL".to_owned());
    }
    let Some(scheme) = web_scheme_len(url) else {
        return Some("Use an http:// or https:// URL".to_owned());
    };
    let rest = url.get(scheme..).unwrap_or_default();
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Some(
            "Leave credentials out of the URL; the key is sent as a bearer token".to_owned(),
        );
    }
    if authority.is_empty() || authority.starts_with(':') || url.chars().any(char::is_whitespace) {
        return Some("Enter a complete URL, such as https://bots.example.com/webhook".to_owned());
    }
    if url.contains('#') {
        return Some("Remove the #fragment".to_owned());
    }
    None
}

/// `host[:port]` of a stored target URL.
fn url_host(url: &str) -> &str {
    let rest = web_scheme_len(url)
        .and_then(|n| url.get(n..))
        .unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.rsplit('@').next().unwrap_or(authority)
}

/// A connect override is a bare host name: no scheme, port or path.
fn connect_problem(host: &str) -> Option<String> {
    let ok = host.is_empty()
        || (host.len() <= 253
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')));
    (!ok).then(|| "A host name only, without scheme, port or path".to_owned())
}

/// `email.received (pipeline=calendar) → Kitchen bot`.
fn route_line(event: &str, arguments: &BTreeMap<String, String>, target: &str) -> String {
    if arguments.is_empty() {
        return format!("{event} \u{2192} {target}");
    }
    let arguments = arguments
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{event} ({arguments}) \u{2192} {target}")
}

/// Only the event's own arguments, trimmed; an empty one matches any value
/// and is left out.
fn route_create(
    event: &RoutableEventView,
    values: &BTreeMap<String, String>,
    target: &str,
) -> EventRouteCreate {
    let arguments = event
        .arguments
        .iter()
        .filter_map(|name| {
            let value = values.get(name)?.trim();
            (!value.is_empty()).then(|| (name.clone(), value.to_owned()))
        })
        .collect();
    EventRouteCreate {
        event: event.name.clone(),
        arguments,
        target: target.to_owned(),
    }
}

fn route_problem(event: &str, target: &str, overview: &OutboundOverview) -> Option<&'static str> {
    if !overview.events.iter().any(|e| e.name == event) {
        Some("Choose an event")
    } else if !overview.targets.iter().any(|t| t.id == target) {
        Some("Choose a target")
    } else {
        None
    }
}

/// Shape and word of a delivery's state, and whether the word is for screen
/// readers only (a delivered row is a quiet dot).
fn delivery_status(state: DeliveryState, attempts: u32) -> (StatusKind, &'static str, bool) {
    match state {
        DeliveryState::Pending if attempts == 0 => (StatusKind::Running, "Pending", false),
        DeliveryState::Pending => (StatusKind::Running, "Retrying", false),
        DeliveryState::Delivered => (StatusKind::Ok, "Delivered", true),
        DeliveryState::Ignored => (StatusKind::Ok, "Ignored", false),
        DeliveryState::Failed => (StatusKind::Fault, "Failed", false),
        DeliveryState::Unconfigured => (StatusKind::Idle, "Unconfigured", false),
        DeliveryState::Skipped => (StatusKind::Idle, "Skipped", false),
    }
}

fn kind_label(kind: DeliveryKind) -> &'static str {
    match kind {
        DeliveryKind::Route => "route",
        DeliveryKind::Notice => "notice",
        DeliveryKind::Subscription => "subscription",
    }
}

/// A route or notice names a target (shown by its label); a subscription
/// names its callback host.
fn destination_name(
    kind: DeliveryKind,
    destination: &str,
    labels: &HashMap<String, String>,
) -> String {
    match kind {
        DeliveryKind::Subscription => destination.to_owned(),
        DeliveryKind::Route | DeliveryKind::Notice => labels
            .get(destination)
            .cloned()
            .unwrap_or_else(|| destination.to_owned()),
    }
}

fn attempts_label(attempts: u32) -> String {
    if attempts == 0 {
        "not tried yet".to_owned()
    } else {
        logic::plural(attempts as usize, "attempt", "attempts")
    }
}

/// `some_code` as `Some code`, for codes this page does not know.
fn humanize(code: &str) -> String {
    let words = code.replace('_', " ");
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn failure_label(code: &str) -> String {
    match code {
        "rejected" => "Refused by the receiver".to_owned(),
        "attempts_exhausted" => "Still failing a day after it was queued".to_owned(),
        other => humanize(other),
    }
}

/// `HTTP 503`, `Timed out`, `Network error`.
fn attempt_outcome(attempt: &AttemptView) -> String {
    match (attempt.status, attempt.error.as_deref()) {
        (_, Some("timeout")) => "Timed out".to_owned(),
        (_, Some("network")) => "Network error".to_owned(),
        (Some(status), _) => format!("HTTP {status}"),
        (None, Some(error)) => match error.strip_prefix("http_") {
            Some(code) => format!("HTTP {code}"),
            None => humanize(error),
        },
        (None, None) => "No answer".to_owned(),
    }
}

fn attempt_failed(attempt: &AttemptView) -> bool {
    attempt.error.is_some() || attempt.status.is_none_or(|s| !(200..300).contains(&s))
}

/// The line under a delivery that needs one: why it failed, how the last
/// try went while it retries, or what it waits for.
fn delivery_note(delivery: &DeliveryView) -> Option<(String, Tone)> {
    let last = delivery.history.first();
    match delivery.state {
        DeliveryState::Failed => {
            let why = delivery
                .failure
                .as_deref()
                .map_or_else(|| "Failed".to_owned(), failure_label);
            let text = match last {
                Some(last) => format!("{why} · last attempt: {}", attempt_outcome(last)),
                None => why,
            };
            Some((text, Tone::Fault))
        }
        DeliveryState::Pending => last.filter(|last| attempt_failed(last)).map(|last| {
            (
                format!("Last attempt: {}", attempt_outcome(last)),
                Tone::Warn,
            )
        }),
        DeliveryState::Unconfigured => Some((
            format!("Sent once a target named {} exists", delivery.destination),
            Tone::Neutral,
        )),
        DeliveryState::Delivered | DeliveryState::Ignored | DeliveryState::Skipped => None,
    }
}

/// The toast after a retry, and whether it reads as success.
fn retry_outcome(delivery: &DeliveryView, destination: &str) -> (String, bool) {
    let event = &delivery.event;
    match delivery.state {
        DeliveryState::Delivered => (format!("Delivered {event} to {destination}"), true),
        DeliveryState::Ignored => (format!("{destination} accepted {event}"), true),
        DeliveryState::Pending => (format!("Sending {event} to {destination} again"), true),
        DeliveryState::Failed => {
            let last = delivery
                .history
                .first()
                .map(|a| format!(": {}", attempt_outcome(a)))
                .unwrap_or_default();
            (
                format!("{event} to {destination} failed again{last}"),
                false,
            )
        }
        DeliveryState::Unconfigured => (
            format!("{event} waits for a target named {destination}"),
            false,
        ),
        DeliveryState::Skipped => (format!("{event} to {destination} was cancelled"), false),
    }
}

#[cfg(test)]
mod tests {
    use omni_api::outbound::DeliveryCounts;

    use super::*;

    fn target(id: &str) -> EventTargetView {
        EventTargetView {
            id: id.into(),
            label: "Kitchen bot".into(),
            url: "https://bots.example.com/hook".into(),
            key_set: true,
            connect: Some("proxy".into()),
            created_at: 1,
            updated_at: 2,
        }
    }

    fn attempt(status: Option<u16>, error: Option<&str>) -> AttemptView {
        AttemptView {
            at: 10,
            status,
            error: error.map(str::to_owned),
            duration_ms: 120,
        }
    }

    fn delivery(state: DeliveryState, history: Vec<AttemptView>) -> DeliveryView {
        DeliveryView {
            id: "d1".into(),
            event: "email.received".into(),
            kind: DeliveryKind::Route,
            destination: "kitchen".into(),
            state,
            attempts: history.len() as u32,
            created_at: 1,
            updated_at: 2,
            next_attempt_at: None,
            failure: None,
            history,
        }
    }

    #[test]
    fn target_ids_follow_the_server_rule() {
        let existing = ["chief".to_owned()];
        assert_eq!(target_id_problem("kitchen-2", &existing), None);
        assert!(target_id_problem("", &existing).is_some());
        assert!(target_id_problem("Kitchen", &existing).is_some());
        assert!(target_id_problem("2kitchen", &existing).is_some());
        assert!(target_id_problem("kitchen_bot", &existing).is_some());
        assert!(target_id_problem(&"a".repeat(33), &existing).is_some());
        assert_eq!(target_id_problem(&"a".repeat(32), &existing), None);
        assert_eq!(
            target_id_problem("chief", &existing).as_deref(),
            Some("A target with this id exists; edit it instead")
        );
    }

    #[test]
    fn target_urls_need_a_web_scheme_a_host_and_no_secrets() {
        assert_eq!(target_url_problem("https://bots.example.com/hook"), None);
        assert_eq!(target_url_problem(" HTTP://kitchen:8080/hook "), None);
        assert!(target_url_problem("").is_some());
        assert!(target_url_problem("ftp://host/x").is_some());
        assert!(target_url_problem("bots.example.com").is_some());
        assert!(target_url_problem("https://user:pw@host/x").is_some());
        assert!(target_url_problem("https:///x").is_some());
        assert!(target_url_problem("https://:443/x").is_some());
        assert!(target_url_problem("https://host/a b").is_some());
        assert!(target_url_problem("https://host/x#frag").is_some());
    }

    #[test]
    fn connect_overrides_are_bare_host_names() {
        assert_eq!(connect_problem(""), None);
        assert_eq!(connect_problem("proxy.compose_default"), None);
        assert!(connect_problem("proxy:443").is_some());
        assert!(connect_problem("https://proxy").is_some());
        assert!(connect_problem(&"a".repeat(254)).is_some());
    }

    #[test]
    fn a_key_is_required_only_on_create_and_empty_optionals_are_left_out() {
        let mut draft = TargetDraft {
            id: "kitchen".into(),
            label: " Kitchen bot ".into(),
            url: "https://bots.example.com/hook".into(),
            ..TargetDraft::default()
        };
        assert!(target_problems(&draft, true, &[]).key.is_some());
        assert!(!target_problems(&draft, false, &["kitchen".into()]).any());
        assert_eq!(
            draft.upsert(),
            EventTargetUpsert {
                label: "Kitchen bot".into(),
                url: "https://bots.example.com/hook".into(),
                key: None,
                connect: None,
            }
        );
        draft.key = " secret ".into();
        draft.connect = "proxy".into();
        assert!(!target_problems(&draft, true, &[]).any());
        let body = draft.upsert();
        assert_eq!(body.key.as_deref(), Some("secret"));
        assert_eq!(body.connect.as_deref(), Some("proxy"));
    }

    #[test]
    fn an_edit_starts_from_the_stored_target_without_its_key() {
        let draft = TargetDraft::of(&target("kitchen"));
        assert_eq!(draft.key, "");
        assert_eq!(draft.connect, "proxy");
        assert_eq!(draft.upsert().connect.as_deref(), Some("proxy"));
    }

    #[test]
    fn url_host_drops_scheme_path_and_credentials() {
        assert_eq!(
            url_host("https://bots.example.com/hook?x=1"),
            "bots.example.com"
        );
        assert_eq!(url_host("http://kitchen:8080"), "kitchen:8080");
        assert_eq!(url_host("https://user@host/x"), "host");
        assert_eq!(url_host("weird"), "weird");
    }

    #[test]
    fn routes_read_as_event_arguments_and_target() {
        let mut arguments = BTreeMap::new();
        assert_eq!(
            route_line("email.received", &arguments, "Kitchen bot"),
            "email.received \u{2192} Kitchen bot"
        );
        arguments.insert("pipeline".to_owned(), "calendar".to_owned());
        arguments.insert("folder".to_owned(), "INBOX".to_owned());
        assert_eq!(
            route_line("email.received", &arguments, "kitchen"),
            "email.received (folder=INBOX, pipeline=calendar) \u{2192} kitchen"
        );
    }

    #[test]
    fn a_new_route_keeps_only_the_events_filled_arguments() {
        let event = RoutableEventView {
            name: "email.received".into(),
            description: "A new email".into(),
            arguments: vec!["pipeline".into(), "folder".into()],
        };
        let values = BTreeMap::from([
            ("pipeline".to_owned(), " calendar ".to_owned()),
            ("folder".to_owned(), "  ".to_owned()),
            ("stale".to_owned(), "x".to_owned()),
        ]);
        let body = route_create(&event, &values, "kitchen");
        assert_eq!(body.event, "email.received");
        assert_eq!(body.target, "kitchen");
        assert_eq!(
            body.arguments,
            BTreeMap::from([("pipeline".to_owned(), "calendar".to_owned())])
        );
    }

    #[test]
    fn a_route_needs_a_known_event_and_target() {
        let overview = OutboundOverview {
            targets: vec![target("kitchen")],
            routes: Vec::new(),
            deliveries: Vec::new(),
            counts: DeliveryCounts::default(),
            events: vec![RoutableEventView {
                name: "email.received".into(),
                description: String::new(),
                arguments: Vec::new(),
            }],
        };
        assert_eq!(
            route_problem("", "kitchen", &overview),
            Some("Choose an event")
        );
        assert_eq!(
            route_problem("email.received", "", &overview),
            Some("Choose a target")
        );
        assert_eq!(
            route_problem("email.received", "gone", &overview),
            Some("Choose a target")
        );
        assert_eq!(route_problem("email.received", "kitchen", &overview), None);
    }

    #[test]
    fn delivery_states_map_to_status_shapes() {
        use DeliveryState::*;
        assert_eq!(
            delivery_status(Pending, 0),
            (StatusKind::Running, "Pending", false)
        );
        assert_eq!(
            delivery_status(Pending, 2),
            (StatusKind::Running, "Retrying", false)
        );
        assert_eq!(
            delivery_status(Delivered, 1),
            (StatusKind::Ok, "Delivered", true)
        );
        assert_eq!(delivery_status(Ignored, 1).0, StatusKind::Ok);
        assert_eq!(delivery_status(Failed, 5).0, StatusKind::Fault);
        assert_eq!(delivery_status(Unconfigured, 0).0, StatusKind::Idle);
        assert_eq!(delivery_status(Skipped, 0).0, StatusKind::Idle);
    }

    #[test]
    fn attempts_read_as_http_status_or_error() {
        assert_eq!(attempt_outcome(&attempt(Some(200), None)), "HTTP 200");
        assert_eq!(
            attempt_outcome(&attempt(Some(403), Some("http_403"))),
            "HTTP 403"
        );
        assert_eq!(
            attempt_outcome(&attempt(None, Some("http_502"))),
            "HTTP 502"
        );
        assert_eq!(
            attempt_outcome(&attempt(None, Some("timeout"))),
            "Timed out"
        );
        assert_eq!(
            attempt_outcome(&attempt(None, Some("network"))),
            "Network error"
        );
        assert_eq!(
            attempt_outcome(&attempt(None, Some("tls_failed"))),
            "Tls failed"
        );
        assert!(!attempt_failed(&attempt(Some(202), None)));
        assert!(attempt_failed(&attempt(Some(500), None)));
        assert!(attempt_failed(&attempt(None, Some("timeout"))));
    }

    #[test]
    fn notes_say_why_a_delivery_failed_retries_or_waits() {
        let mut failed = delivery(
            DeliveryState::Failed,
            vec![attempt(Some(403), Some("http_403"))],
        );
        failed.failure = Some("rejected".into());
        assert_eq!(
            delivery_note(&failed),
            Some((
                "Refused by the receiver · last attempt: HTTP 403".to_owned(),
                Tone::Fault
            ))
        );
        let retrying = delivery(DeliveryState::Pending, vec![attempt(None, Some("timeout"))]);
        assert_eq!(
            delivery_note(&retrying),
            Some(("Last attempt: Timed out".to_owned(), Tone::Warn))
        );
        assert_eq!(
            delivery_note(&delivery(DeliveryState::Pending, Vec::new())),
            None
        );
        assert_eq!(
            delivery_note(&delivery(DeliveryState::Unconfigured, Vec::new())).map(|(_, t)| t),
            Some(Tone::Neutral)
        );
        assert_eq!(
            delivery_note(&delivery(
                DeliveryState::Delivered,
                vec![attempt(Some(200), None)]
            )),
            None
        );
    }

    #[test]
    fn retry_toasts_say_how_the_retry_went() {
        let ok = delivery(DeliveryState::Delivered, vec![attempt(Some(200), None)]);
        assert_eq!(
            retry_outcome(&ok, "Kitchen bot"),
            ("Delivered email.received to Kitchen bot".to_owned(), true)
        );
        let again = delivery(DeliveryState::Failed, vec![attempt(Some(500), None)]);
        assert_eq!(
            retry_outcome(&again, "Kitchen bot"),
            (
                "email.received to Kitchen bot failed again: HTTP 500".to_owned(),
                false
            )
        );
        assert!(retry_outcome(&delivery(DeliveryState::Pending, Vec::new()), "x").1);
    }

    #[test]
    fn destinations_use_target_labels_except_for_subscriptions() {
        let labels = HashMap::from([("kitchen".to_owned(), "Kitchen bot".to_owned())]);
        assert_eq!(
            destination_name(DeliveryKind::Route, "kitchen", &labels),
            "Kitchen bot"
        );
        assert_eq!(
            destination_name(DeliveryKind::Notice, "gone", &labels),
            "gone"
        );
        assert_eq!(
            destination_name(DeliveryKind::Subscription, "kitchen", &labels),
            "kitchen"
        );
        assert_eq!(attempts_label(0), "not tried yet");
        assert_eq!(attempts_label(1), "1 attempt");
    }
}
