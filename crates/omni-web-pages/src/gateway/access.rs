//! Who may connect: OAuth sign-ins waiting for approval, client tokens
//! (shown once at creation) and approved OAuth grants.

use leptos::prelude::*;
use omni_api::gateway::{ClientTokenCreate, ClientTokenView, OAuthGrantView};
use omni_web_kit::api;
use omni_web_kit::components::{
    Button, ButtonSize, ButtonVariant, ConfirmButton, EmptyState, Icon, Panel, Tag, ToastKind, Tone,
};
use omni_web_kit::hooks::use_now;
use omni_web_kit::task::spawn_detached;
use omni_web_kit::utils::format::{format_absolute, format_countdown};

use super::endpoint::page_origin;
use super::logic::{self, MAX_LABEL_CHARS};
use super::{CopyButton, Gateway};

#[component]
pub(super) fn AccessColumn(gw: Gateway) -> impl IntoView {
    let has_pending = Memo::new(move |_| {
        gw.overview
            .with(|o| o.as_ref().is_some_and(|o| !o.pending.is_empty()))
    });
    view! {
        <section class="stack gw-access" aria-label="Access">
            {move || has_pending.get().then(|| view! { <PendingPanel gw/> })}
            <TokensPanel gw/>
            <GrantsPanel gw/>
        </section>
    }
}

// ----- pending authorizations -----

#[component]
fn PendingPanel(gw: Gateway) -> impl IntoView {
    // Countdowns tick every second while anything waits.
    let now = use_now(1000);
    let ids = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| o.pending.iter().map(|p| p.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let deciding = RwSignal::new(None::<String>);
    view! {
        <Panel
            title="Waiting for approval"
            id="gateway-pending"
            class="gw-pending"
            head_end=ViewFn::from(move || view! { <span class="num">{move || ids.with(Vec::len)}</span> })
        >
            <p class="panel-body small dim gw-pending-help">
                "Approve only a sign-in you started: type the code its page shows. A request you did not start is someone else's; deny it."
            </p>
            <div class="rows">
                <For
                    each=move || ids.get()
                    key=|id| id.clone()
                    children=move |id| view! { <PendingRow id gw now deciding/> }
                />
            </div>
        </Panel>
    }
}

#[component]
fn PendingRow(
    id: String,
    gw: Gateway,
    now: ReadSignal<f64>,
    deciding: RwSignal<Option<String>>,
) -> impl IntoView {
    let key = StoredValue::new(id);
    let item = Memo::new(move |_| {
        gw.overview.with(|o| {
            key.with_value(|id| o.as_ref()?.pending.iter().find(|p| p.id == *id).cloned())
        })
    });
    let expired = Memo::new(move |_| {
        item.with(|p| {
            p.as_ref()
                .is_some_and(|p| (p.expires_at as f64) <= now.get())
        })
    });
    let busy = Signal::derive(move || {
        deciding.with(|d| key.with_value(|id| d.as_deref() == Some(id.as_str())))
    });
    let typed = RwSignal::new(String::new());
    let code_ready = Memo::new(move |_| {
        typed.with(|code| code.chars().filter(|c| c.is_ascii_alphanumeric()).count() == 8)
    });
    let decide = move |approve: bool| {
        if deciding.get_untracked().is_some() || (approve && !code_ready.get_untracked()) {
            return;
        }
        let id = key.get_value();
        let code = typed.get_untracked();
        let client = item
            .with_untracked(|p| p.as_ref().map(|p| p.client_name.clone()))
            .unwrap_or_default();
        deciding.set(Some(id.clone()));
        spawn_detached(async move {
            let outcome = if approve {
                api::approve_gateway_authorization(&id, &code).await
            } else {
                api::deny_gateway_authorization(&id).await
            };
            match outcome {
                Ok(()) => {
                    gw.toast.show(
                        if approve {
                            format!("Approved {client}; it can connect now")
                        } else {
                            format!("Denied {client}")
                        },
                        ToastKind::Info,
                    );
                    gw.overview.try_update(|o| {
                        if let Some(o) = o {
                            o.pending.retain(|p| p.id != id);
                        }
                    });
                }
                Err(e) => gw.toast.show(e.message().to_owned(), ToastKind::Error),
            }
            deciding.try_set(None);
            gw.reload();
        });
    };
    move || {
        let p = item.get()?;
        let host = logic::client_host(&p.client_id);
        let verified = p.verified;
        let input_id = format!("gw-code-{}", p.id);
        let label_for = input_id.clone();
        let name = p.client_name.clone();
        let deny_name = name.clone();
        let expires = p.expires_at as f64;
        let created = p.created_at as f64;
        Some(view! {
            <div class="row gw-pending-row">
                <span class="row-main">
                    <span class="row-title">
                        {p.client_name.clone()}
                        {(!verified).then(|| view! {
                            <Tag tone=Tone::Warn title="This app registered itself; its name is not verified">"unverified"</Tag>
                        })}
                    </span>
                    <span class="row-sub mono" title=p.client_id.clone()>
                        {format!("{host} · returns to {}", p.redirect_host)}
                    </span>
                    <span class="row-sub num">
                        <span title=format_absolute(created)>{move || format!("asked {}", logic::ago(created, now.get()))}</span>
                        " · "
                        <span class=move || if expired.get() { "text-warn" } else { "" } title=format_absolute(expires)>
                            {move || {
                                let left = expires - now.get();
                                if left <= 0.0 { "expired".to_owned() } else { format!("expires in {}", format_countdown(left)) }
                            }}
                        </span>
                    </span>
                </span>
                <label class="gw-code-entry" for=label_for>
                    <span class="small dim">"Code from the sign-in page"</span>
                    <input
                        id=input_id
                        class="input mono gw-code-input"
                        type="text"
                        inputmode="text"
                        autocomplete="off"
                        autocapitalize="characters"
                        spellcheck="false"
                        maxlength="11"
                        placeholder="XXXX-XXXX"
                        prop:value=move || typed.get()
                        on:input=move |ev| typed.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" { decide(true) }
                    />
                </label>
                <span class="row-end gw-decide">
                    <Button
                        variant=ButtonVariant::Primary
                        icon=Icon::Check
                        busy
                        disabled=Signal::derive(move || expired.get() || !code_ready.get())
                        disabled_reason="Type the 8-character code from the sign-in page"
                        aria_label=format!("Approve {name}")
                        on_click=Callback::new(move |_| decide(true))
                    >
                        "Approve"
                    </Button>
                    <Button
                        variant=ButtonVariant::Ghost
                        disabled=busy
                        disabled_reason="Deciding"
                        aria_label=format!("Deny {deny_name}")
                        on_click=Callback::new(move |_| decide(false))
                    >
                        "Deny"
                    </Button>
                </span>
            </div>
        })
    }
}

// ----- client tokens -----

#[component]
fn TokensPanel(gw: Gateway) -> impl IntoView {
    let ids = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| o.clients.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let creating = RwSignal::new(false);
    let label = RwSignal::new(String::new());
    let attempted = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // The new token lives only here, until "Done" (or leaving the page).
    let revealed = RwSignal::new(None::<(String, String)>);
    let problem = Memo::new(move |_| label.with(|l| logic::label_problem(l)));
    let revoking = RwSignal::new(None::<String>);

    let create = move || {
        if saving.get_untracked() {
            return;
        }
        attempted.set(true);
        if problem.get_untracked().is_some() {
            return;
        }
        let body = ClientTokenCreate {
            label: label.get_untracked().trim().to_owned(),
        };
        saving.set(true);
        error.set(None);
        spawn_detached(async move {
            match api::create_gateway_client(&body).await {
                Ok(created) => {
                    let client = created.client.clone();
                    revealed.try_set(Some((client.label.clone(), created.token)));
                    creating.try_set(false);
                    label.try_set(String::new());
                    attempted.try_set(false);
                    gw.overview.try_update(|o| {
                        if let Some(o) = o
                            && !o.clients.iter().any(|c| c.id == client.id)
                        {
                            o.clients.push(client);
                        }
                    });
                    gw.reload();
                }
                Err(e) => {
                    error.try_set(Some(e.message().to_owned()));
                }
            }
            saving.try_set(false);
        });
    };
    let revoke = Callback::new(move |id: String| {
        let name = gw.overview.with_untracked(|o| {
            o.as_ref()
                .and_then(|o| {
                    o.clients
                        .iter()
                        .find(|c| c.id == id)
                        .map(|c| c.label.clone())
                })
                .unwrap_or_default()
        });
        revoking.set(Some(id.clone()));
        spawn_detached(async move {
            match api::revoke_gateway_client(&id).await {
                Ok(()) => {
                    gw.toast.show(format!("Revoked {name}"), ToastKind::Info);
                    gw.overview.try_update(|o| {
                        if let Some(o) = o {
                            o.clients.retain(|c| c.id != id);
                        }
                    });
                    gw.reload();
                }
                Err(e) => gw.toast.show(e.message().to_owned(), ToastKind::Error),
            }
            revoking.try_set(None);
        });
    });

    view! {
        <Panel
            title="Client tokens"
            class="gw-tokens"
            head_end=ViewFn::from(move || view! {
                <span class="num">{move || ids.with(Vec::len)}</span>
                <Button
                    size=ButtonSize::Sm
                    icon=Icon::Plus
                    disabled=Signal::derive(move || creating.get())
                    disabled_reason="Finish the token below first"
                    on_click=Callback::new(move |_| {
                        revealed.set(None);
                        creating.set(true);
                    })
                >
                    "New token"
                </Button>
            })
        >
            {move || revealed.get().map(|(name, token)| view! {
                <TokenReveal gw name token on_done=Callback::new(move |()| revealed.set(None))/>
            })}
            {move || creating.get().then(|| view! {
                <form
                    class="panel-body field gw-new-token"
                    on:submit=move |event| {
                        event.prevent_default();
                        create();
                    }
                >
                    <label class="field-label" for="gw-token-label">"Who is it for?"</label>
                    <div class="cluster token-row">
                        <input
                            id="gw-token-label"
                            class="input"
                            type="text"
                            autocomplete="off"
                            maxlength=MAX_LABEL_CHARS.to_string()
                            placeholder="Laptop Claude Code"
                            aria-invalid=move || (attempted.get() && problem.with(Option::is_some)).then_some("true")
                            prop:value=move || label.get()
                            on:input=move |ev| label.set(event_target_value(&ev))
                        />
                        <Button submit=true busy=saving>"Create"</Button>
                        <Button
                            variant=ButtonVariant::Ghost
                            on_click=Callback::new(move |_| {
                                creating.set(false);
                                error.set(None);
                                attempted.set(false);
                            })
                        >
                            "Cancel"
                        </Button>
                    </div>
                    {move || attempted.get().then(|| problem.get()).flatten().map(|e| view! { <p class="field-error">{e}</p> })}
                    {move || error.get().map(|e| view! { <p class="field-error" role="alert">{e}</p> })}
                </form>
            })}
            {move || if ids.with(Vec::is_empty) {
                view! {
                    <EmptyState compact=true message="No client tokens yet. Create one for Claude Code or Codex."/>
                }
                .into_any()
            } else {
                view! {
                    <div class="rows">
                        <For
                            each=move || ids.get()
                            key=|id| id.clone()
                            children=move |id| view! { <TokenRow id gw revoking revoke/> }
                        />
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}

/// The new token, once: copy it or the finished Claude Code command.
#[component]
fn TokenReveal(gw: Gateway, name: String, token: String, on_done: Callback<()>) -> impl IntoView {
    let url = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| {
                    o.public_url
                        .clone()
                        .unwrap_or_else(|| logic::lan_url(&page_origin(), &o.path))
                })
                .unwrap_or_default()
        })
    });
    let secret = StoredValue::new(token);
    let token_text = Signal::derive(move || secret.get_value());
    let command =
        Signal::derive(move || secret.with_value(|t| logic::claude_code_command(&url.get(), t)));
    view! {
        <div class="panel-body gw-secret" role="region" aria-label="New client token">
            <p class="gw-secret-title">{format!("Token for \u{201c}{name}\u{201d}")}</p>
            <p class="small">"Copy it now. Omni cannot show it again; if it is lost, revoke it and create another."</p>
            <div class="cluster token-row">
                <label class="sr-only" for="gw-new-token-value">"Client token"</label>
                <input
                    id="gw-new-token-value"
                    class="input mono"
                    type="text"
                    readonly=true
                    autocomplete="off"
                    spellcheck="false"
                    prop:value=move || token_text.get()
                />
                <CopyButton text=token_text label="Copy token" text_label=true variant=ButtonVariant::Secondary/>
            </div>
            <div class="cluster">
                <CopyButton text=command label="Copy Claude Code command" text_label=true/>
                <span class="spacer"></span>
                <Button on_click=Callback::new(move |_| on_done.run(()))>"Done"</Button>
            </div>
        </div>
    }
}

fn last_used(at: Option<i64>, now: f64) -> String {
    at.map_or_else(
        || "never used".to_owned(),
        |at| format!("used {}", logic::ago(at as f64, now)),
    )
}

#[component]
fn TokenRow(
    id: String,
    gw: Gateway,
    revoking: RwSignal<Option<String>>,
    revoke: Callback<String>,
) -> impl IntoView {
    let key = StoredValue::new(id);
    let item = Memo::new(move |_| {
        gw.overview.with(|o| {
            key.with_value(|id| o.as_ref()?.clients.iter().find(|c| c.id == *id).cloned())
        })
    });
    let busy = Signal::derive(move || {
        revoking.with(|r| key.with_value(|id| r.as_deref() == Some(id.as_str())))
    });
    move || {
        let ClientTokenView {
            label,
            prefix,
            created_at,
            last_used_at,
            ..
        } = item.get()?;
        let created = created_at as f64;
        Some(view! {
            <div class="row">
                <span class="row-main">
                    <span class="row-title">{label.clone()}</span>
                    <span class="row-sub parts">
                        <span class="mono">{format!("{prefix}…")}</span>
                        <span class="dot-sep">
                            <span title=format_absolute(created)>{move || format!("created {}", logic::ago(created, gw.now.get()))}</span>
                            <span title=last_used_at.map(|at| format_absolute(at as f64))>{move || last_used(last_used_at, gw.now.get())}</span>
                        </span>
                    </span>
                </span>
                <span class="row-end">
                    <ConfirmButton
                        label="Revoke"
                        confirm_label="Revoke token"
                        context=format!("token {label}")
                        destructive=true
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Sm
                        busy
                        title=format!("Clients using {label} stop connecting at once")
                        on_confirm=Callback::new(move |()| revoke.run(key.get_value()))
                    />
                </span>
            </div>
        })
    }
}

// ----- OAuth grants -----

#[component]
fn GrantsPanel(gw: Gateway) -> impl IntoView {
    let ids = Memo::new(move |_| {
        gw.overview.with(|o| {
            o.as_ref()
                .map(|o| o.grants.iter().map(|g| g.id.clone()).collect::<Vec<_>>())
                .unwrap_or_default()
        })
    });
    let oauth = Memo::new(move |_| {
        gw.overview
            .with(|o| o.as_ref().is_some_and(|o| o.oauth_available))
    });
    let revoking = RwSignal::new(None::<String>);
    let revoke = Callback::new(move |id: String| {
        let name = gw.overview.with_untracked(|o| {
            o.as_ref()
                .and_then(|o| {
                    o.grants
                        .iter()
                        .find(|g| g.id == id)
                        .map(|g| g.client_name.clone())
                })
                .unwrap_or_default()
        });
        revoking.set(Some(id.clone()));
        spawn_detached(async move {
            match api::revoke_gateway_grant(&id).await {
                Ok(()) => {
                    gw.toast.show(format!("Revoked {name}"), ToastKind::Info);
                    gw.overview.try_update(|o| {
                        if let Some(o) = o {
                            o.grants.retain(|g| g.id != id);
                        }
                    });
                    gw.reload();
                }
                Err(e) => gw.toast.show(e.message().to_owned(), ToastKind::Error),
            }
            revoking.try_set(None);
        });
    });
    view! {
        <Panel
            title="OAuth grants"
            class="gw-grants"
            head_end=ViewFn::from(move || view! { <span class="num">{move || ids.with(Vec::len)}</span> })
        >
            {move || if ids.with(Vec::is_empty) {
                let message = if oauth.get() {
                    "No OAuth clients yet. ChatGPT appears here once you approve its sign-in."
                } else {
                    "OAuth clients such as ChatGPT need the public URL: set OMNI_GATEWAY_PUBLIC_URL first."
                };
                view! { <EmptyState compact=true message/> }.into_any()
            } else {
                view! {
                    <div class="rows">
                        <For
                            each=move || ids.get()
                            key=|id| id.clone()
                            children=move |id| view! { <GrantRow id gw revoking revoke/> }
                        />
                    </div>
                }
                .into_any()
            }}
        </Panel>
    }
}

#[component]
fn GrantRow(
    id: String,
    gw: Gateway,
    revoking: RwSignal<Option<String>>,
    revoke: Callback<String>,
) -> impl IntoView {
    let key = StoredValue::new(id);
    let item = Memo::new(move |_| {
        gw.overview
            .with(|o| key.with_value(|id| o.as_ref()?.grants.iter().find(|g| g.id == *id).cloned()))
    });
    let busy = Signal::derive(move || {
        revoking.with(|r| key.with_value(|id| r.as_deref() == Some(id.as_str())))
    });
    move || {
        let OAuthGrantView {
            client_name,
            client_id,
            created_at,
            last_used_at,
            scope,
            ..
        } = item.get()?;
        let created = created_at as f64;
        let host = logic::client_host(&client_id);
        Some(view! {
            <div class="row">
                <span class="row-main">
                    <span class="row-title">{client_name.clone()}</span>
                    <span class="row-sub parts">
                        <span class="mono truncate" title=client_id.clone()>{host}</span>
                        <span class="dot-sep">
                            <span title=format_absolute(created)>{move || format!("approved {}", logic::ago(created, gw.now.get()))}</span>
                            <span title=last_used_at.map(|at| format_absolute(at as f64))>{move || last_used(last_used_at, gw.now.get())}</span>
                        </span>
                    </span>
                    {scope.map(|scope| view! { <span class="cluster"><Tag title="Granted scope">{scope}</Tag></span> })}
                </span>
                <span class="row-end">
                    <ConfirmButton
                        label="Revoke"
                        confirm_label="Revoke access"
                        context=format!("access for {client_name}")
                        destructive=true
                        variant=ButtonVariant::Ghost
                        size=ButtonSize::Sm
                        busy
                        title=format!("{client_name} loses access and must sign in again")
                        on_confirm=Callback::new(move |()| revoke.run(key.get_value()))
                    />
                </span>
            </div>
        })
    }
}
